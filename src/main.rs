mod cli;
mod cmux;
mod config;
mod llm;
mod mc_data;
mod session;
mod tui;

use crate::cmux::client::CmuxClient;
use crate::cmux::events;
use crate::config::{Cli, Config};
use crate::llm::Summarizer;
use crate::llm::codex::CodexSummarizer;
use crate::llm::openai::OpenAISummarizer;
use crate::llm::typesafe::TypeSafeClassifier;
use crate::session::watcher::SessionWatcher;
use crate::tui::app::App;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};

enum AppControl {
    Quit,
    Reload,
}

struct BinaryStamp {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl BinaryStamp {
    fn capture() -> Option<Self> {
        let path = std::env::current_exe().ok()?;
        let metadata = std::fs::metadata(&path).ok()?;
        Some(Self {
            path,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    fn has_changed(&self) -> bool {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return false;
        };

        metadata.len() != self.len || metadata.modified().ok() != self.modified
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => run_tui(cli.tui).await,
        Some(config::Command::Resolve { workspace_id }) => cli::resolve::run(&workspace_id),
        Some(config::Command::Setup) => cli::setup::run(),
    }
}

async fn run_tui(_tui_config: Config) -> Result<()> {
    let binary_stamp = BinaryStamp::capture();

    loop {
        // Re-parse argv each iteration so a soft reload picks up any env-var
        // changes (e.g. OPENAI_API_KEY) — matches pre-subcommand behavior.
        let config = config::Cli::parse().tui;
        let cmux_client = CmuxClient::new(config.cmux_bin.clone(), config.cmux_socket.clone());

        // Prefer Codex (local auth, no API key) when use_codex is set,
        // fall back to OpenAI if explicitly requested or as a backup.
        let summarizer: Option<Arc<dyn Summarizer>> = if config.use_codex {
            Some(Arc::new(CodexSummarizer::new(
                config.codex_bin.clone(),
                config::SUMMARIZE_PROMPT.to_string(),
                None, // use codex's default model
            )) as Arc<dyn Summarizer>)
        } else {
            config.openai_api_key.as_ref().map(|key| {
                Arc::new(OpenAISummarizer::new(
                    key.clone(),
                    config.model.clone(),
                    config::SUMMARIZE_PROMPT.to_string(),
                )) as Arc<dyn Summarizer>
            })
        };

        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let classifier = config
            .typesafe_api_key
            .as_ref()
            .map(|key| TypeSafeClassifier::new(key.clone()));

        let result = run_app(
            &mut terminal,
            &config,
            &cmux_client,
            summarizer,
            classifier.as_ref(),
        )
        .await;

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        match result? {
            AppControl::Quit => return Ok(()),
            AppControl::Reload => {
                if binary_stamp.as_ref().is_some_and(BinaryStamp::has_changed) {
                    restart_current_process()?;
                }
            }
        }
    }
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &Config,
    cmux_client: &CmuxClient,
    summarizer: Option<Arc<dyn Summarizer>>,
    classifier: Option<&TypeSafeClassifier>,
) -> Result<AppControl> {
    let mut app = App::new();
    app.refresh_workspaces(cmux_client, &config.histories_dir)
        .await?;

    // Channel for async screen-capture results (per-workspace, parallel)
    let (screen_tx, mut screen_rx) = mpsc::unbounded_channel::<crate::tui::app::ScreenUpdate>();

    // Kick off initial screen capture for the selected workspace
    app.spawn_load_screen_preview(cmux_client.clone(), classifier.cloned(), screen_tx.clone());

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
    let mut screen_interval = interval(Duration::from_secs(15));

    loop {
        terminal.draw(|f| {
            // Split vertically: main area on top, single-line shortcut footer on the bottom.
            let vchunks =
                Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).split(f.area());

            let chunks =
                Layout::horizontal([Constraint::Length(32), Constraint::Min(40)]).split(vchunks[0]);

            let sidebar_focused = app.focus == crate::tui::app::Focus::Sidebar;
            tui::sidebar::render_sidebar(
                f,
                chunks[0],
                &app.workspaces,
                app.selected,
                sidebar_focused,
            );
            tui::detail::render_detail(
                f,
                chunks[1],
                app.selected_workspace(),
                app.detail_scroll,
                !sidebar_focused,
            );
            tui::footer::render_footer(f, vchunks[1], app.focus);
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
                            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                                return Ok(AppControl::Reload);
                            }
                            (KeyCode::Char('j') | KeyCode::Down, _) => {
                                if app.focus == crate::tui::app::Focus::Detail {
                                    app.scroll_down();
                                } else {
                                    app.next();
                                    app.spawn_load_screen_preview(
                                        cmux_client.clone(),
                                        classifier.cloned(),
                                        screen_tx.clone(),
                                    );
                                }
                            }
                            (KeyCode::Char('k') | KeyCode::Up, _) => {
                                if app.focus == crate::tui::app::Focus::Detail {
                                    app.scroll_up();
                                } else {
                                    app.previous();
                                    app.spawn_load_screen_preview(
                                        cmux_client.clone(),
                                        classifier.cloned(),
                                        screen_tx.clone(),
                                    );
                                }
                            }
                            (KeyCode::Char('l') | KeyCode::Right, _) | (KeyCode::Enter, KeyModifiers::NONE) => {
                                if app.focus == crate::tui::app::Focus::Sidebar {
                                    app.focus = crate::tui::app::Focus::Detail;
                                } else {
                                    // In detail focus, Enter switches to the workspace in cmux (fire-and-forget)
                                    if let Some(ws) = app.selected_workspace() {
                                        let client = cmux_client.clone();
                                        let ref_id = ws.workspace.ref_id.clone();
                                        tokio::spawn(async move {
                                            let _ = client.select_workspace(&ref_id).await;
                                        });
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
                                // Refresh screen preview (async)
                                app.spawn_load_screen_preview(
                                    cmux_client.clone(),
                                    classifier.cloned(),
                                    screen_tx.clone(),
                                );
                            }
                            (KeyCode::Char('n'), _) => {
                                // Open notes for current workspace in $EDITOR
                                if let Some(ws) = app.selected_workspace() {
                                    let notes_path = app.notes_path_for(ws);
                                    if let Some(parent) = notes_path.parent() {
                                        let _ = std::fs::create_dir_all(parent);
                                    }
                                    // Suspend TUI
                                    disable_raw_mode()?;
                                    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                                    terminal.show_cursor()?;

                                    let editor = std::env::var("EDITOR")
                                        .unwrap_or_else(|_| "vim".to_string());
                                    let _ = std::process::Command::new(&editor)
                                        .arg(&notes_path)
                                        .status();

                                    // Resume TUI
                                    enable_raw_mode()?;
                                    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                                    terminal.clear()?;
                                    app.load_notes();
                                }
                            }
                            (KeyCode::Char('r'), _) => {
                                // Summarize the selected workspace.
                                // We do a DEEP capture (500 lines of scrollback) so codex
                                // sees the actual conversation trajectory, not just the
                                // last 15 lines of trailing terminal output.
                                if let Some(ws) = app.workspaces.get(app.selected) {
                                    let uuid = ws.workspace.uuid.clone();
                                    let ref_id = ws.workspace.ref_id.clone();
                                    let ws_name = ws.workspace.name.clone();
                                    let notes = ws.notes.clone().unwrap_or_default();
                                    let session_bullets = ws
                                        .session
                                        .as_ref()
                                        .map(|s| s.bullets.join("\n"))
                                        .unwrap_or_default();
                                    if let Some(ref summarizer) = summarizer {
                                        app.set_summarizing(&uuid);
                                        let summarizer = Arc::clone(summarizer);
                                        let tx = summary_tx.clone();
                                        let client = cmux_client.clone();
                                        let uuid_for_task = uuid.clone();
                                        tokio::spawn(async move {
                                            // Deep scrollback capture for real context
                                            let scrollback = tokio::time::timeout(
                                                std::time::Duration::from_secs(5),
                                                client.read_screen(&ref_id, 500),
                                            )
                                            .await
                                            .ok()
                                            .and_then(|r| r.ok())
                                            .unwrap_or_default();

                                            let context = build_summary_context(
                                                &ws_name,
                                                &scrollback,
                                                &session_bullets,
                                                &notes,
                                            );

                                            match summarizer.summarize(&context).await {
                                                Ok(summary) => {
                                                    let _ = tx.send((uuid_for_task, summary));
                                                }
                                                Err(e) => {
                                                    let msg: String = format!("{:#}", e)
                                                        .chars()
                                                        .take(220)
                                                        .collect();
                                                    let _ = tx.send((
                                                        uuid_for_task,
                                                        crate::llm::Summary {
                                                            trajectory: format!("Summary failed: {}", msg),
                                                            next_steps: vec![],
                                                        },
                                                    ));
                                                }
                                            }
                                        });
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

            _ = screen_interval.tick() => {
                // Fire off parallel background captures for every workspace.
                // The main loop never blocks — results flow back via screen_rx.
                app.spawn_refresh_all_screens(
                    cmux_client.clone(),
                    classifier.cloned(),
                    screen_tx.clone(),
                );
            }

            Some(update) = screen_rx.recv() => {
                app.apply_screen_update(update);
            }
        }

        if app.should_quit {
            return Ok(AppControl::Quit);
        }
    }
}

fn restart_current_process() -> Result<()> {
    let current_exe = std::env::current_exe()?;
    let err = std::process::Command::new(current_exe)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(err.into())
}

/// Assemble the summarization context from all available signals.
/// Order matters — most relevant signal first so even a small context window
/// gets the right material.
fn build_summary_context(
    workspace_name: &str,
    scrollback: &str,
    session_bullets: &str,
    notes: &str,
) -> String {
    let mut parts = Vec::new();

    parts.push(format!("# Workspace: {}", workspace_name));

    if !notes.trim().is_empty() {
        parts.push(format!("\n## My Notes\n{}", notes.trim()));
    }

    if !session_bullets.trim().is_empty() {
        parts.push(format!(
            "\n## Recent activity log (session bullets, newest last)\n{}",
            session_bullets.trim()
        ));
    }

    if !scrollback.trim().is_empty() {
        // Strip empty lines and the most aggressive whitespace to compress
        let cleaned: String = scrollback
            .lines()
            .map(|l| l.trim_end())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!(
            "\n## Terminal scrollback (most recent conversation, oldest first)\n{}",
            cleaned
        ));
    }

    parts.join("\n")
}
