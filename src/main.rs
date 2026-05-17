mod cmux;
mod config;
mod llm;
mod session;
mod tui;

use crate::cmux::client::CmuxClient;
use crate::cmux::events;
use crate::config::Config;
use crate::llm::openai::OpenAISummarizer;
use crate::llm::Summarizer;
use crate::session::watcher::SessionWatcher;
use crate::tui::app::App;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    Terminal,
};
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();
    let cmux_client = CmuxClient::new(config.cmux_bin.clone(), config.cmux_socket.clone());

    let summarizer: Option<Arc<dyn Summarizer>> = config.openai_api_key.as_ref().map(|key| {
        Arc::new(OpenAISummarizer::new(
            key.clone(),
            config.model.clone(),
            config::SUMMARIZE_PROMPT.to_string(),
        )) as Arc<dyn Summarizer>
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &config, &cmux_client, summarizer).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &Config,
    cmux_client: &CmuxClient,
    summarizer: Option<Arc<dyn Summarizer>>,
) -> Result<()> {
    let mut app = App::new();
    app.refresh_workspaces(cmux_client, &config.histories_dir)
        .await?;
    app.load_screen_preview(cmux_client).await;

    // Spawn cmux event stream subscriber
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cmux_bin = config.cmux_bin.clone();
    let cmux_socket = config.cmux_socket.clone();
    tokio::spawn(async move {
        let _ = events::subscribe(&cmux_bin, &cmux_socket, event_tx).await;
    });

    // Create session file watcher
    let (file_tx, mut file_rx) = mpsc::unbounded_channel();
    let _watcher = SessionWatcher::new(config.histories_dir.clone(), file_tx)?;

    // Channel for LLM summary completions
    let (summary_tx, mut summary_rx) = mpsc::unbounded_channel::<(String, crate::llm::Summary)>();

    let mut refresh_interval = interval(Duration::from_secs(30));

    loop {
        terminal.draw(|f| {
            let chunks = Layout::horizontal([Constraint::Length(32), Constraint::Min(40)])
                .split(f.area());

            let sidebar_focused = app.focus == crate::tui::app::Focus::Sidebar;
            tui::sidebar::render_sidebar(f, chunks[0], &app.workspaces, app.selected, sidebar_focused);
            tui::detail::render_detail(f, chunks[1], app.selected_workspace(), app.detail_scroll, !sidebar_focused);
        })?;

        tokio::select! {
            // Poll terminal events on a 50ms tick
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                while event::poll(std::time::Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        // Only process key press events (not release/repeat)
                        if key.kind != KeyEventKind::Press {
                            continue;
                        }
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('q'), _)
                            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                app.should_quit = true;
                            }
                            (KeyCode::Char('j') | KeyCode::Down, _) => {
                                if app.focus == crate::tui::app::Focus::Detail {
                                    app.scroll_down();
                                } else {
                                    app.next();
                                    app.load_screen_preview(cmux_client).await;
                                }
                            }
                            (KeyCode::Char('k') | KeyCode::Up, _) => {
                                if app.focus == crate::tui::app::Focus::Detail {
                                    app.scroll_up();
                                } else {
                                    app.previous();
                                    app.load_screen_preview(cmux_client).await;
                                }
                            }
                            (KeyCode::Char('l') | KeyCode::Right, _) | (KeyCode::Enter, KeyModifiers::NONE) => {
                                if app.focus == crate::tui::app::Focus::Sidebar {
                                    app.focus = crate::tui::app::Focus::Detail;
                                } else {
                                    // In detail focus, Enter switches to the workspace in cmux
                                    if let Some(ws) = app.selected_workspace() {
                                        let _ = cmux_client
                                            .select_workspace(&ws.workspace.ref_id)
                                            .await;
                                    }
                                }
                            }
                            (KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc, _) => {
                                if app.focus == crate::tui::app::Focus::Detail {
                                    app.focus = crate::tui::app::Focus::Sidebar;
                                    app.detail_scroll = 0;
                                }
                            }
                            (KeyCode::Char('s'), _) => {
                                // Refresh screen preview
                                app.load_screen_preview(cmux_client).await;
                            }
                            (KeyCode::Char('r'), _) => {
                                if let Some(ws) = app.workspaces.get(app.selected) {
                                    if let Some(ref session) = ws.session {
                                        let uuid = ws.workspace.uuid.clone();
                                        let context = session.bullets.join("\n");
                                        if let Some(ref summarizer) = summarizer {
                                            let summarizer = Arc::clone(summarizer);
                                            let tx = summary_tx.clone();
                                            tokio::spawn(async move {
                                                if let Ok(summary) =
                                                    summarizer.summarize(&context).await
                                                {
                                                    let _ = tx.send((uuid, summary));
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            Some(agent_event) = event_rx.recv() => {
                let ws_uuid = agent_event.workspace_id.clone();
                app.handle_agent_event(&agent_event);

                if app.needs_summary(&ws_uuid, config.summary_threshold) {
                    app.reset_tool_count(&ws_uuid);
                    if let Some(ref summarizer) = summarizer {
                        if let Some(&idx) = app.workspace_index_for(&ws_uuid) {
                            if let Some(ref session) = app.workspaces[idx].session {
                                let context = session.bullets.join("\n");
                                let summarizer = Arc::clone(summarizer);
                                let tx = summary_tx.clone();
                                let uuid = ws_uuid.clone();
                                tokio::spawn(async move {
                                    if let Ok(summary) =
                                        summarizer.summarize(&context).await
                                    {
                                        let _ = tx.send((uuid, summary));
                                    }
                                });
                            }
                        }
                    }
                }
            }

            Some(changed) = file_rx.recv() => {
                if let Some(ws_uuid) = app.handle_file_changed(&changed) {
                    if let Some(ref summarizer) = summarizer {
                        if let Some(&idx) = app.workspace_index_for(&ws_uuid) {
                            if let Some(ref session) = app.workspaces[idx].session {
                                let context = session.bullets.join("\n");
                                let summarizer = Arc::clone(summarizer);
                                let tx = summary_tx.clone();
                                let uuid = ws_uuid.clone();
                                tokio::spawn(async move {
                                    if let Ok(summary) =
                                        summarizer.summarize(&context).await
                                    {
                                        let _ = tx.send((uuid, summary));
                                    }
                                });
                            }
                        }
                    }
                }
            }

            Some((uuid, summary)) = summary_rx.recv() => {
                app.apply_summary(&uuid, summary.clone());
                if let Some(&idx) = app.workspace_index_for(&uuid) {
                    if let Some(ref session) = app.workspaces[idx].session {
                        let mut updated = session.clone();
                        updated.trajectory = Some(summary.trajectory);
                        updated.next_steps = summary.next_steps;
                        let _ = updated.write();
                    }
                }
            }

            _ = refresh_interval.tick() => {
                let _ = app.refresh_workspaces(cmux_client, &config.histories_dir).await;
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
