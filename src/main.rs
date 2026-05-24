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
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
};
use std::collections::HashMap;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};

/// A trajectory.md file was written externally — carry just the workspace uuid.
#[derive(Debug, Clone)]
struct TrajectoryUpdate {
    uuid: String,
}

/// Start a recursive notify watcher on `dir` (creating it first if missing).
/// Returns `(watcher_handle, receiver)`. The watcher_handle must be kept alive.
///
/// Debounce: consecutive events for the same uuid within 100 ms are collapsed.
/// Pattern matched: `<dir>/<uuid>/trajectory.md`.
fn start_trajectory_watcher(
    dir: PathBuf,
) -> anyhow::Result<(RecommendedWatcher, mpsc::UnboundedReceiver<TrajectoryUpdate>)> {
    // Ensure the watched directory exists so the watcher never panics on
    // first-time-user setups.
    std::fs::create_dir_all(&dir)?;

    let (tx, rx) = mpsc::unbounded_channel::<TrajectoryUpdate>();

    // Debounce table: uuid -> last-sent Instant (shared via Mutex so the
    // notify callback (non-async closure) can mutate it).
    let debounce: std::sync::Arc<std::sync::Mutex<HashMap<String, Instant>>> =
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

    let mut watcher = notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            let Ok(event) = res else { return };
            match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) => {}
                _ => return,
            }
            for path in &event.paths {
                // Match: .../<uuid>/trajectory.md
                let file_name = path.file_name().and_then(|n| n.to_str());
                if file_name != Some("trajectory.md") {
                    continue;
                }
                let uuid = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string());
                let Some(uuid) = uuid else { continue };

                // Debounce: skip if another event for this uuid was sent < 100ms ago.
                let now = Instant::now();
                let mut table = debounce.lock().unwrap();
                if let Some(&last) = table.get(&uuid) {
                    if now.duration_since(last).as_millis() < 100 {
                        continue;
                    }
                }
                table.insert(uuid.clone(), now);
                drop(table);

                let _ = tx.send(TrajectoryUpdate { uuid });
            }
        },
    )?;

    watcher.watch(&dir, RecursiveMode::Recursive)?;

    Ok((watcher, rx))
}

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
        Some(config::Command::PromoteRules { proposals_file }) => cli::promote_rules::run(&proposals_file),
        Some(config::Command::RecordHit { project, rule_id }) => cli::record_hit::run(&project, &rule_id),
        Some(config::Command::Gc) => cli::gc::run(),
        Some(config::Command::Bind { surface_id, session_file }) =>
            cli::bind::run(&surface_id, session_file.as_deref()),
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

    // Create trajectory file watcher — watches ~/data/mission-control/.data/ recursively.
    // start_trajectory_watcher mkdir-p's the dir so it never panics for first-time users.
    let data_subroot = crate::mc_data::paths::data_subroot();
    let (mut traj_rx, _traj_watcher_opt) = match start_trajectory_watcher(data_subroot) {
        Ok((_watcher, rx)) => {
            // Keep the watcher alive for the duration of run_app.
            (rx, Some(_watcher))
        }
        Err(e) => {
            eprintln!("trajectory watcher: {e:?}");
            // Degrade gracefully: use an idle channel. The 30 s loop still works.
            let (_dead_tx, dead_rx) = mpsc::unbounded_channel::<TrajectoryUpdate>();
            (dead_rx, None)
        }
    };

    // Channel for LLM summary completions
    let (summary_tx, mut summary_rx) = mpsc::unbounded_channel::<(String, crate::llm::Summary)>();

    // Channel for trajectory regen completions: (uuid, Result<TrajectoryDoc, String>)
    let (regen_tx, mut regen_rx) = mpsc::channel::<(String, Result<crate::mc_data::trajectory::TrajectoryDoc, String>)>(16);

    // Channel for surface summary completions: (uuid, sid, summary)
    let (surface_summary_tx, mut surface_summary_rx) = mpsc::channel::<(String, String, String)>(32);

    // Channel for dismissal results: (uuid, Result<DismissalArtifacts, String>)
    let (dismiss_tx, mut dismiss_rx) = mpsc::channel::<(String, Result<crate::mc_data::dismissal::DismissalArtifacts, String>)>(8);

    let mut refresh_interval = interval(Duration::from_secs(30));
    let mut screen_interval = interval(Duration::from_secs(15));
    let mut regen_tick = interval(Duration::from_secs(30));
    let mut surface_summary_tick = interval(Duration::from_secs(60));
    let mut dismiss_tick = interval(Duration::from_secs(30));

    // Track per-workspace surface count from the previous cmux refresh.
    // Used to detect surface detachments between cmux event stream events.
    let mut prev_surface_counts: HashMap<String, u32> = HashMap::new();

    // Channel for peek-mode screen-poll results: (workspace_uuid, screen_text)
    let (peek_tx, mut peek_rx) = mpsc::channel::<(String, String)>(64);

    // Tick for peek-mode polling (~200ms; cheap because peek_needs_poll is a no-op
    // unless a workspace is actually in peek mode and past its poll deadline).
    let mut peek_tick = tokio::time::interval(Duration::from_millis(200));
    peek_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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

                        // ── Trajectory key routing ────────────────────────────
                        // When in Detail focus and the selected workspace has a
                        // trajectory loaded, intercept keys for the editor.
                        {
                            use crate::tui::trajectory_edit::EditMode;
                            let in_detail = app.focus == crate::tui::app::Focus::Detail;
                            let has_traj = app
                                .selected_workspace()
                                .map_or(false, |ws| ws.trajectory.is_some());

                            if in_detail && has_traj {
                                let in_insert = app
                                    .selected_workspace()
                                    .and_then(|ws| ws.edit_state.as_ref())
                                    .map_or(false, |s| {
                                        matches!(s.mode, EditMode::Insert { .. })
                                    });

                                let is_traj_nav_key = matches!(
                                    key.code,
                                    KeyCode::Char('j')
                                        | KeyCode::Down
                                        | KeyCode::Char('k')
                                        | KeyCode::Up
                                        | KeyCode::Char('g')
                                        | KeyCode::Char('G')
                                        | KeyCode::Char('i')
                                        | KeyCode::Enter
                                        | KeyCode::Char(' ')
                                        | KeyCode::Char('x')
                                        | KeyCode::Char('d')
                                        | KeyCode::Char('o')
                                        | KeyCode::Char('O')
                                        | KeyCode::Char('J')
                                        | KeyCode::Char('K')
                                );

                                if in_insert || is_traj_nav_key {
                                    let actions = app.handle_trajectory_key(key);
                                    if !actions.is_empty() {
                                        if let Err(e) = app.save_trajectory_edits(&actions) {
                                            eprintln!("save_trajectory_edits: {e:?}");
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                        // ─────────────────────────────────────────────────────

                        // If a dismissal confirmation was pending and the user
                        // pressed anything other than D, cancel the pending state.
                        if app.pending_dismissal_workspace().is_some()
                            && !matches!(key.code, KeyCode::Char('D'))
                        {
                            app.clear_pending_dismissal();
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
                                    // Auto-save in-flight trajectory edits if in insert mode before
                                    // switching workspace (commit_insert produces the actions).
                                    let in_insert = app
                                        .selected_workspace()
                                        .and_then(|ws| ws.edit_state.as_ref())
                                        .map_or(false, |s| {
                                            matches!(
                                                s.mode,
                                                crate::tui::trajectory_edit::EditMode::Insert { .. }
                                            )
                                        });
                                    if in_insert {
                                        if let Err(e) = app.save_trajectory_edits(&[]) {
                                            eprintln!("auto-save on switch: {e:?}");
                                        }
                                    }
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
                                    // Auto-save in-flight trajectory edits if in insert mode before
                                    // switching workspace (commit_insert produces the actions).
                                    let in_insert = app
                                        .selected_workspace()
                                        .and_then(|ws| ws.edit_state.as_ref())
                                        .map_or(false, |s| {
                                            matches!(
                                                s.mode,
                                                crate::tui::trajectory_edit::EditMode::Insert { .. }
                                            )
                                        });
                                    if in_insert {
                                        if let Err(e) = app.save_trajectory_edits(&[]) {
                                            eprintln!("auto-save on switch: {e:?}");
                                        }
                                    }
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
                            (KeyCode::Char('D'), _) => {
                                // Manual dismissal with D-D confirmation: first D sets
                                // pending state; second D on the same workspace executes.
                                if app.focus == crate::tui::app::Focus::Sidebar {
                                    if let Some(ws) = app.selected_workspace() {
                                        let uuid = ws.workspace.uuid.clone();
                                        app.handle_dismissal_request(&uuid);
                                        // If executed (true), dismissal is now in flight.
                                        // If pending (false), user must press D again to confirm.
                                    }
                                }
                            }
                            (KeyCode::Char('R'), _) => {
                                // Shift+R in detail focus: force a trajectory regen on the
                                // next scheduler tick, bypassing event/time thresholds.
                                if app.focus == crate::tui::app::Focus::Detail {
                                    app.force_regen_selected_workspace();
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
                // Accumulate events for the regen scheduler.
                app.increment_regen_event_count(&ws_uuid);

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

            Some(update) = traj_rx.recv() => {
                app.apply_trajectory_update(&update.uuid);
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
                // After refresh, diff surface counts to detect detachments.
                // cmux doesn't yet emit surface.opened/surface.closed events, so
                // we poll on each refresh tick (every 30 s).
                let surface_diffs: Vec<(String, u32)> = app
                    .workspaces
                    .iter()
                    .filter_map(|ws| {
                        let uuid = ws.workspace.uuid.clone();
                        let new_count = ws.surfaces.len() as u32;
                        let old_count = prev_surface_counts.get(&uuid).copied();
                        prev_surface_counts.insert(uuid.clone(), new_count);
                        if old_count.is_none() || old_count == Some(new_count) {
                            None
                        } else {
                            Some((uuid, new_count))
                        }
                    })
                    .collect();
                for (uuid, count) in surface_diffs {
                    app.set_open_surfaces(&uuid, count);
                }
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

            _ = peek_tick.tick() => {
                if let Some((uuid, surface_ref)) = app.peek_needs_poll() {
                    // Owned copies for the spawned task.
                    let uuid = uuid.to_string();
                    let surface_ref = surface_ref.to_string();
                    // Check whether this is an agent-log source or a shell source.
                    let uses_cmux = app.workspaces.iter()
                        .find(|ws| ws.workspace.uuid == uuid)
                        .and_then(|ws| ws.peek_state.as_ref())
                        .map_or(false, |p| p.uses_cmux_screen());
                    app.mark_peek_polling();
                    if uses_cmux {
                        // Shell source: poll cmux read-screen in a background task.
                        let client = cmux_client.clone();
                        let tx = peek_tx.clone();
                        tokio::spawn(async move {
                            let result = tokio::time::timeout(
                                Duration::from_secs(5),
                                client.read_screen(&surface_ref, 100),
                            )
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                            .unwrap_or_default();
                            let _ = tx.send((uuid, result)).await;
                        });
                    } else {
                        // Agent source: read the session log synchronously and
                        // rebuild the buffer directly. No cmux call needed.
                        app.refresh_agent_peek_buffer(&uuid);
                    }
                }
            }

            Some((uuid, screen_text)) = peek_rx.recv() => {
                app.apply_peek_screen_update(&uuid, screen_text);
            }

            _ = regen_tick.tick() => {
                // Check each workspace to see if a trajectory regen is due.
                if let Some(ref summarizer) = summarizer {
                    let due = app.workspaces_due_for_regen();
                    for uuid in due {
                        if let Some(inputs) = app.build_regen_inputs(&uuid) {
                            app.mark_regen_in_flight(&uuid);
                            let summarizer = Arc::clone(summarizer);
                            let tx = regen_tx.clone();
                            let uuid_for_task = uuid.clone();
                            tokio::spawn(async move {
                                match crate::llm::trajectory_regen::regenerate(&summarizer, &inputs).await {
                                    Ok(doc) => {
                                        let _ = tx.send((uuid_for_task, Ok(doc))).await;
                                    }
                                    Err(e) => {
                                        let _ = tx.send((uuid_for_task, Err(format!("{:#}", e)))).await;
                                    }
                                }
                            });
                        }
                    }
                }
            }

            Some((uuid, result)) = regen_rx.recv() => {
                match result {
                    Ok(doc) => app.apply_regenerated_trajectory(&uuid, doc),
                    Err(e) => eprintln!("regen({uuid}): {e}"),
                }
            }

            _ = surface_summary_tick.tick() => {
                // Check for shell surfaces due for LLM summarization.
                if let Some(ref summarizer) = summarizer {
                    let due = app.surfaces_due_for_summary();
                    for (uuid, sid, log_path) in due {
                        // Read last 15 lines from the log file.
                        let recent_commands: Vec<String> = match std::fs::read_to_string(&log_path) {
                            Ok(content) => content
                                .lines()
                                .rev()
                                .take(15)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .map(|s| s.to_string())
                                .collect(),
                            Err(_) => continue,
                        };
                        if recent_commands.is_empty() {
                            continue;
                        }
                        let summarizer = Arc::clone(summarizer);
                        let tx = surface_summary_tx.clone();
                        let uuid_for_task = uuid.clone();
                        let sid_for_task = sid.clone();
                        tokio::spawn(async move {
                            let inputs = crate::llm::surface_summary::SurfaceSummaryInputs {
                                kind: "shell".to_string(),
                                cwd: String::new(), // no cwd from log path alone
                                recent_commands,
                            };
                            match crate::llm::surface_summary::summarize(&summarizer, &inputs).await {
                                Ok(summary) => {
                                    let _ = tx.send((uuid_for_task, sid_for_task, summary)).await;
                                }
                                Err(e) => {
                                    eprintln!("surface_summary({uuid_for_task}/{sid_for_task}): {e}");
                                }
                            }
                        });
                    }
                }
            }

            Some((uuid, sid, summary)) = surface_summary_rx.recv() => {
                app.apply_surface_summary(&uuid, &sid, summary);
            }

            _ = dismiss_tick.tick() => {
                // Check for workspaces whose grace timer has elapsed.
                let due = app.workspaces_ready_for_dismissal(Duration::from_secs(300));
                for uuid in due {
                    app.mark_dismissing(&uuid);
                    let inputs = app.build_learning_inputs(&uuid);
                    let tx = dismiss_tx.clone();
                    // If we have a summarizer, run the learning LLM call.
                    // If not, skip the LLM call and finalize with a placeholder record.
                    let summarizer_opt = summarizer.clone();
                    tokio::spawn(async move {
                        let learning_result = if let Some(ref summarizer) = summarizer_opt {
                            crate::llm::learning::produce_learning(summarizer, &inputs).await
                        } else {
                            Ok(crate::llm::learning::LearningOutputs {
                                full_record_md: format!(
                                    "# Workspace record: {}\n\n(No LLM configured — record generated without learning extraction.)\n",
                                    inputs.workspace_name
                                ),
                                candidates_only_md: None,
                            })
                        };
                        match learning_result {
                            Ok(out) => {
                                match crate::mc_data::dismissal::finalize(
                                    &inputs.workspace_uuid,
                                    &out.full_record_md,
                                    out.candidates_only_md.as_deref(),
                                ) {
                                    Ok(artifacts) => {
                                        let _ = tx.send((inputs.workspace_uuid, Ok(artifacts))).await;
                                    }
                                    Err(e) => {
                                        let _ = tx.send((inputs.workspace_uuid, Err(format!("{:#}", e)))).await;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send((inputs.workspace_uuid, Err(format!("LLM error: {:#}", e)))).await;
                            }
                        }
                    });
                }
            }

            Some((uuid, result)) = dismiss_rx.recv() => {
                match result {
                    Ok(artifacts) => {
                        eprintln!(
                            "dismissed {uuid}: archived to {:?}, published to {:?}",
                            artifacts.local_archive, artifacts.obsidian_record
                        );
                        app.drop_dismissed_workspace(&uuid);
                    }
                    Err(e) => {
                        eprintln!("dismiss failed for {uuid}: {e}");
                        // For v1: log and leave dismissing=true so we don't retry
                        // automatically (avoids infinite retry loops on permanent errors).
                        // A future enhancement could unset dismissing for transient errors.
                    }
                }
            }
        }

        // Check for peek yield (Enter pressed in peek mode) after every select!
        // iteration. This is async but resolves immediately (no I/O in the
        // hot path) and keeps the redraw path unblocked.
        if let Some(workspace_ref) = app.take_peek_yield() {
            if let Err(e) = cmux_client.select_workspace(&workspace_ref).await {
                eprintln!("peek-yield select_workspace({workspace_ref}): {e:?}");
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
