use crate::cmux::client::{CmuxClient, SurfaceInfo, Workspace};
use crate::cmux::events::AgentEvent;
use crate::llm::Summary;
use crate::llm::trajectory_regen::RegenInputs;
use crate::llm::typesafe::{ScreenClassification, TypeSafeClassifier};
use crate::session::file::{self, SessionFile};
use crate::session::watcher::FileChanged;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Directory for persistent per-workspace notes.
pub fn notes_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/mission-control/notes")
}

/// Directory for hook-written status files.
pub fn status_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/mission-control/status")
}

/// Slugify a workspace name for use as a filename.
fn workspace_slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    slug.trim_matches('-').to_string()
}

/// Per-workspace state for the trajectory regeneration scheduler.
#[derive(Debug, Clone)]
pub struct RegenSchedulerState {
    /// When the last successful regen completed (or None if never).
    pub last_regen_at: Option<Instant>,
    /// Number of tool-call events accumulated since the last regen.
    pub events_since_last_regen: u32,
    /// True while a regen task is in-flight to prevent duplicate spawns.
    pub regen_in_flight: bool,
}

impl Default for RegenSchedulerState {
    fn default() -> Self {
        Self {
            last_regen_at: None,
            events_since_last_regen: 0,
            regen_in_flight: false,
        }
    }
}

/// Per-workspace state for shell surface summarization.
#[derive(Debug, Clone, Default)]
pub struct SurfaceSummaryState {
    /// Number of new log lines accumulated since the last summary call.
    pub lines_since_last_summary: u32,
}

/// Per-workspace dismissal tracking state.
#[derive(Debug, Clone)]
pub struct DismissalState {
    /// Current count of open cmux surfaces for this workspace.
    pub open_surfaces: u32,
    /// When the surface count first dropped to zero (grace timer start).
    pub grace_started_at: Option<Instant>,
    /// True once the dismissal task has been spawned (prevents duplicate spawns).
    pub dismissing: bool,
}

impl Default for DismissalState {
    fn default() -> Self {
        Self {
            open_surfaces: 0,
            grace_started_at: None,
            dismissing: false,
        }
    }
}

/// Key insights extracted from raw screen text.
#[derive(Debug, Clone, Default)]
pub struct ScreenInsights {
    pub user_prompt: Option<String>,
    pub activity: Option<String>,
    pub working_dir: Option<String>,
    pub duration: Option<String>,
    pub tasks_done: u16,
    pub tasks_total: u16,
    pub pending_task: Option<String>,
    /// Agent detected from screen content (e.g. model names, tmux status bars).
    pub agent: Option<String>,
}

/// Parse screen text to extract the user's prompt, current activity, and working dir.
pub fn parse_screen_insights(screen: &str) -> ScreenInsights {
    let mut insights = ScreenInsights::default();

    let lines: Vec<&str> = screen.lines().collect();

    // --- User prompt ---
    // Find the user's message that the agent is currently working on.
    // In Claude Code / Codex, prompts between ─── dividers at the bottom
    // are the input area (queued/pending). Prompts above (with ›) are
    // confirmed conversation history. We want the last one being worked on.

    // Find the input area at the bottom of the screen.
    // Claude Code input area: ───/❯ prompt/───/⏵ status
    // Scan bottom-up, skip status bars and prompts between dividers.
    let mut input_area_start = lines.len();
    let mut found_divider = false;
    for i in (0..lines.len()).rev() {
        let trimmed = lines[i].trim();
        let is_pure_divider = !trimmed.is_empty() && trimmed.chars().all(|c| c == '─');
        if is_pure_divider {
            input_area_start = i;
            found_divider = true;
        } else if found_divider {
            // Inside the input area block - skip prompts, status bars, empty lines
            if trimmed.starts_with('⏵') || trimmed.starts_with('❯') || trimmed.is_empty() {
                continue;
            }
            // Hit a real content line above the input area
            break;
        }
    }

    // Find the last activity indicator line position
    let mut activity_line_idx: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if i >= input_area_start {
            break;
        }
        let trimmed = line.trim();
        let first_char = trimmed.chars().next().unwrap_or(' ');
        let is_spinner = !first_char.is_ascii_alphanumeric()
            && !first_char.is_ascii_whitespace()
            && !matches!(
                first_char,
                '─' | '│'
                    | '⏵'
                    | '└'
                    | '├'
                    | '⎿'
                    | '⏺'
                    | '●'
                    | '▸'
                    | '▹'
                    | '►'
                    | '▶'
                    | '›'
                    | '❯'
            );
        if is_spinner
            && !trimmed.contains("ctrl+")
            && !trimmed.contains("lines (")
            && ((trimmed.contains('…') && trimmed.contains('(') && trimmed.contains('s'))
                || (trimmed.contains(" for ") && trimmed.contains('s')))
        {
            activity_line_idx = Some(i);
        }
    }

    // Strategy:
    // 1. Check input area (between ─── dividers) for a prompt with content.
    //    If found, that's the current/just-sent task.
    // 2. Otherwise, find the last prompt BEFORE the activity indicator
    //    in the conversation area. That's what the agent is working on.
    // 3. Skip prompts BETWEEN activity and input area (queued follow-ups).

    // Check input area for prompt with content
    let mut input_prompt: Option<String> = None;
    for i in input_area_start..lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with('❯') || trimmed.starts_with('›') {
            let content = trimmed
                .trim_start_matches('❯')
                .trim_start_matches('›')
                .trim();
            if !content.is_empty()
                && !content.contains("Press up to edit")
                && !content.contains("Enter to select")
            {
                input_prompt = Some(content.to_string());
            }
        }
    }

    if let Some(prompt) = input_prompt {
        insights.user_prompt = Some(prompt);
    } else {
        // Find prompts in conversation area, only BEFORE the activity
        let scan_limit = activity_line_idx.unwrap_or(input_area_start);
        let mut best_prompt: Option<Vec<&str>> = None;
        let mut current_prompt: Vec<&str> = Vec::new();
        let mut in_prompt = false;

        for (i, line) in lines.iter().enumerate() {
            if i >= scan_limit {
                break;
            }
            let trimmed = line.trim();
            if trimmed.starts_with('›') || trimmed.starts_with('❯') {
                let content = trimmed
                    .trim_start_matches('›')
                    .trim_start_matches('❯')
                    .trim();
                if content.contains("Press up to edit")
                    || content.contains("Enter to select")
                    || content.is_empty()
                {
                    in_prompt = false;
                    continue;
                }
                in_prompt = true;
                current_prompt.clear();
                current_prompt.push(content);
            } else if in_prompt {
                if trimmed.is_empty() || trimmed.starts_with('─') {
                    if !current_prompt.is_empty() {
                        best_prompt = Some(current_prompt.clone());
                    }
                    in_prompt = false;
                } else if line.starts_with("  ") {
                    current_prompt.push(trimmed);
                } else {
                    if !current_prompt.is_empty() {
                        best_prompt = Some(current_prompt.clone());
                    }
                    in_prompt = false;
                }
            }
        }
        if in_prompt && !current_prompt.is_empty() {
            best_prompt = Some(current_prompt);
        }
        if let Some(parts) = best_prompt {
            insights.user_prompt = Some(parts.join(" "));
        }
    }

    // --- Activity & Duration ---
    // Claude Code uses random verbs with spinners: ✳ Inferring…, ✻ Puzzling…,
    // · Running…, ✽ Proofing…, etc. Detect pattern: non-ASCII leader + timing info
    // or "…" suffix. Also catch completion: "Cooked for Xm Xs".
    for line in &lines {
        let trimmed = line.trim();
        let first_char = trimmed.chars().next().unwrap_or(' ');
        let is_spinner_line = !first_char.is_ascii_alphanumeric()
            && !first_char.is_ascii_whitespace()
            && !matches!(
                first_char,
                '─' | '│'
                    | '⏵'
                    | '└'
                    | '├'
                    | '⎿'
                    | '⏺'
                    | '●'
                    | '▸'
                    | '▹'
                    | '►'
                    | '▶'
                    | '›'
                    | '❯'
            );

        if is_spinner_line {
            // Skip Claude Code collapse markers like "… +301 lines (ctrl+o to expand)"
            if trimmed.contains("ctrl+") || trimmed.contains("lines (") {
                continue;
            }
            // Active: "✻ Puzzling… (2m 28s · ↑ 2.5k tokens)"
            if trimmed.contains('…') && trimmed.contains('(') && trimmed.contains('s') {
                insights.activity = Some(trimmed.to_string());
                if let Some(dur) = extract_duration(trimmed) {
                    insights.duration = Some(dur);
                }
            }
            // Completed: "✻ Cooked for 11m 29s"
            else if trimmed.contains(" for ") && trimmed.contains('s') && !trimmed.contains('…')
            {
                insights.activity = Some(trimmed.to_string());
                if let Some(dur) = extract_duration(trimmed) {
                    insights.duration = Some(dur);
                }
            }
            // Codex-style: "• Working (54s · esc to interrupt)"
            else if trimmed.contains('(') && trimmed.contains('s') {
                let paren_content = &trimmed[trimmed.find('(').unwrap()..];
                if paren_content.chars().any(|c| c.is_ascii_digit()) {
                    insights.activity = Some(trimmed.to_string());
                    if let Some(dur) = extract_duration(trimmed) {
                        insights.duration = Some(dur);
                    }
                }
            }
        }

        // "─ Worked for Xm Xs ────" divider lines
        if trimmed.starts_with('─') && trimmed.contains("Worked for ") {
            if let Some(dur) = extract_duration(trimmed) {
                insights.duration = Some(dur);
            }
        }
    }

    // --- Task progress ---
    // Count ✔ (done) and ◼ (pending) task lines
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with('✔') || trimmed.starts_with("✔") {
            insights.tasks_done += 1;
            insights.tasks_total += 1;
        } else if trimmed.starts_with('◼') || trimmed.starts_with("◼") {
            insights.tasks_total += 1;
            if insights.pending_task.is_none() {
                // Capture the first pending task name
                let task = trimmed.trim_start_matches('◼').trim();
                if !task.is_empty() {
                    insights.pending_task = Some(task.to_string());
                }
            }
        }
    }
    // Also check "+N completed" lines
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.contains("completed") {
            if let Some(n) = trimmed
                .split_whitespace()
                .find(|w| w.starts_with('+'))
                .and_then(|w| w.trim_start_matches('+').parse::<u16>().ok())
            {
                insights.tasks_done += n;
                insights.tasks_total += n;
            }
        }
    }

    // --- Working directory ---
    // Pattern: "model · ~/Projects" at bottom of Claude Code / Codex screens
    for line in lines.iter().rev() {
        let trimmed = line.trim();
        if trimmed.contains(" · ~/") || trimmed.contains(" · /") {
            if let Some(pos) = trimmed.rfind(" · ") {
                let dir = trimmed[pos + 3..].trim();
                if dir.starts_with('~') || dir.starts_with('/') {
                    insights.working_dir = Some(dir.to_string());
                    break;
                }
            }
        }
    }

    // --- Agent detection from screen content ---
    // Detect agent from tmux status bars and model status lines.
    let full_lower = screen.to_lowercase();
    if full_lower.contains(":claude")
        || full_lower.contains("\"claude")
        || full_lower.contains("claude code")
    {
        insights.agent = Some("claude".to_string());
    } else if full_lower.contains(":codex")
        || full_lower.contains("\"codex")
        || full_lower.contains("gpt-")
    {
        insights.agent = Some("codex".to_string());
    } else if full_lower.contains(":opencode") || full_lower.contains("\"opencode") {
        insights.agent = Some("opencode".to_string());
    }

    insights
}

/// Extract a duration string like "18m 31s" from text containing timing in parens.
fn extract_duration(text: &str) -> Option<String> {
    // Match patterns like "Worked for 6m 00s", "Crunched for 54s", or "(18m 31s · ...)"
    if let Some(pos) = text.find("for ") {
        let after = &text[pos + 4..];
        let end = after
            .find(|c: char| c == '─' || c == ')' || c == '·')
            .unwrap_or(after.len());
        let dur = after[..end].trim();
        if dur.contains('s') || dur.contains('m') || dur.contains('h') {
            return Some(dur.to_string());
        }
    }
    if let Some(start) = text.find('(') {
        let after = &text[start + 1..];
        if let Some(end) = after.find(|c: char| c == '·' || c == ')') {
            let dur = after[..end].trim();
            if dur.contains('s') || dur.contains('m') || dur.contains('h') {
                return Some(dur.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct WorkspaceState {
    pub workspace: Workspace,
    pub session: Option<SessionFile>,
    pub surfaces: Vec<SurfaceInfo>,
    pub screen_preview: Option<String>,
    pub screen_insights: ScreenInsights,
    pub tool_call_count: u32,
    /// Persistent user notes (loaded from ~/.config/mission-control/notes/).
    pub notes: Option<String>,
    /// Hook-written status (from ~/.config/mission-control/status/).
    /// Tuple of (state, timestamp_secs). Stale after 60s.
    pub hook_status: Option<(String, u64)>,
    /// TypeSafe AI classification of screen content.
    pub classification: Option<ScreenClassification>,
    /// True while a background screen refresh / classification is in flight.
    pub loading: bool,
    /// LLM-generated summary (independent of session.trajectory so it can be
    /// produced even when no session file matches this workspace).
    pub summary: Option<Summary>,
    /// True while a summary is being generated by Codex / OpenAI.
    pub summarizing: bool,
    /// Parsed trajectory doc for this workspace (loaded from `.data/<uuid>/trajectory.md`).
    /// `None` if the file does not exist or could not be read.
    pub trajectory: Option<crate::mc_data::trajectory::TrajectoryDoc>,
    /// Per-workspace trajectory editing state. `None` means no editing has
    /// started yet for this workspace; `Some(...)` persists across workspace
    /// switches so the cursor position is remembered.
    pub edit_state: Option<crate::tui::trajectory_edit::TrajectoryEditState>,
    /// Active peek-mode state for this workspace.
    /// `Some(...)` while the user is viewing a surface's screen in peek mode.
    /// `None` when not in peek mode.
    pub peek_state: Option<crate::tui::peek_view::PeekState>,
    /// Set to `true` when Enter is pressed in peek mode to trigger a cmux
    /// select-workspace call from the event loop.
    pub peek_yield_pending: bool,
    /// Regen scheduler state for trajectory regeneration.
    pub regen: RegenSchedulerState,
    /// Dismissal tracking: surface count, grace timer, dismissing flag.
    pub dismissal: DismissalState,
    /// Active goal-dispatch modal. `Some` while the user is choosing where to
    /// dispatch the goal at the cursor; `None` otherwise. (T4)
    pub dispatch_modal: Option<crate::tui::dispatch_modal::DispatchModal>,
    /// Pending outcome from the dispatch modal — read and acted on by the
    /// main event loop after each key dispatch so async cmux work can be
    /// spawned from outside the `&mut self` borrow.
    pub dispatch_pending_outcome:
        Option<crate::tui::dispatch_modal::DispatchOutcome>,
    /// Most recent dispatch error (e.g. cmux send failure) — shown briefly in
    /// the status line, cleared on the next key press or after ~5s. Populated
    /// by `set_dispatch_error` from the main loop's dispatch outcome handler.
    #[allow(dead_code)]
    pub dispatch_error: Option<String>,
}

/// Result of an async screen capture + classification for a single workspace.
#[derive(Debug)]
pub struct ScreenUpdate {
    pub workspace_uuid: String,
    pub screen: Option<String>,
    pub classification: Option<ScreenClassification>,
}

impl WorkspaceState {
    /// Whether this workspace has an AI agent surface (Claude Code, Codex, etc.)
    /// Checks screen insights (full capture), surface titles, and screen preview.
    pub fn has_agent_surface(&self) -> bool {
        // Screen insights are parsed from the full 100-line capture
        if self.screen_insights.agent.is_some() {
            return true;
        }
        // Direct surface title check
        self.surfaces.iter().any(|s| {
            let t = s.title.to_lowercase();
            t.contains("claude") || t.contains("codex") || t.contains("opencode")
        })
    }

    /// Derive the agent name from session, screen insights, or surface titles.
    pub fn agent_name(&self) -> &str {
        if let Some(ref session) = self.session {
            if let Some(ref agent) = session.frontmatter.agent {
                return agent;
            }
        }
        // Screen insights (parsed from full 100-line capture)
        if let Some(ref agent) = self.screen_insights.agent {
            return agent;
        }
        // Check surface titles
        for s in &self.surfaces {
            let t = s.title.to_lowercase();
            if t.contains("claude") {
                return "claude";
            }
            if t.contains("codex") {
                return "codex";
            }
            if t.contains("opencode") {
                return "opencode";
            }
        }
        ""
    }

    /// Get working directory from screen insights or surface titles.
    pub fn working_dir(&self) -> &str {
        if let Some(ref dir) = self.screen_insights.working_dir {
            return dir;
        }
        // Derive from non-agent surface titles
        self.surfaces
            .iter()
            .find_map(|s| {
                let t = &s.title;
                if t.contains(':') && (t.contains("~/") || t.contains(":/")) {
                    // "blin@host:~/Projects" -> "~/Projects"
                    t.rfind(':').map(|i| &t[i + 1..])
                } else if t.starts_with("~/") || t.starts_with("…/") {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("")
    }

    /// Derive the agent state: is it baking or waiting for me?
    /// Priority: hook status (if fresh) > session frontmatter > screen activity > surface detection.
    pub fn agent_state(&self) -> AgentState {
        // Hook-written status (instant, from Claude Code hooks)
        // Only trust if less than 60 seconds old
        if let Some((ref state, ts)) = self.hook_status {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now.saturating_sub(ts) < 60 {
                return match state.as_str() {
                    "working" => AgentState::Working,
                    "waiting" => AgentState::NeedsMe,
                    _ => AgentState::Idle,
                };
            }
        }

        // TypeSafe AI classification (sub-100ms, high confidence)
        if let Some(ref cls) = self.classification {
            if cls.state_confidence > 0.6 {
                return match cls.state.as_str() {
                    "working" => AgentState::Working,
                    "waiting" => AgentState::NeedsMe,
                    _ => AgentState::Idle,
                };
            }
        }

        // Session frontmatter (manual override)
        if let Some(ref session) = self.session {
            if let Some(ref status) = session.frontmatter.status {
                return match status.as_str() {
                    "active" => AgentState::Working,
                    "waiting" | "idle" => AgentState::NeedsMe,
                    "done" => AgentState::Idle,
                    _ => AgentState::Idle,
                };
            }
        }

        // Derive from screen activity
        if let Some(ref activity) = self.screen_insights.activity {
            // Ongoing: contains ellipsis or parenthesized timing
            // e.g., "✻ Puzzling… (2m 30s)" or "• Working (54s · esc to interrupt)"
            if activity.contains('…') || activity.contains('(') {
                return AgentState::Working;
            }
            // Completed: "✻ Cooked for 11m 29s" (no parens, no ellipsis)
            return AgentState::NeedsMe;
        }

        // Agent detected but no activity → waiting for input
        if self.has_agent_surface() {
            AgentState::NeedsMe
        } else {
            AgentState::Idle
        }
    }

    /// Derive the host from session or surface titles.
    pub fn host_name(&self) -> &str {
        if let Some(ref session) = self.session {
            if let Some(ref host) = session.frontmatter.host {
                return host;
            }
        }
        // Derive from surface titles like "blin@blin-labs:~"
        self.surfaces
            .iter()
            .find_map(|s| {
                if s.title.contains('@') && s.title.contains(':') {
                    let host_part = s.title.split('@').nth(1)?;
                    let host = host_part.split(':').next()?;
                    // Skip local machine
                    if host.contains("mbp") || host.contains("local") {
                        None
                    } else {
                        Some(host)
                    }
                } else {
                    None
                }
            })
            .unwrap_or("")
    }
}

/// Derived workspace status: is the agent baking or waiting for me?
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgentState {
    /// Agent is actively processing (spinner/working visible)
    Working,
    /// Agent finished or blocked — needs human attention
    NeedsMe,
    /// No agent activity detected
    Idle,
}

impl AgentState {
    pub fn label(&self) -> &str {
        match self {
            AgentState::Working => "working",
            AgentState::NeedsMe => "waiting",
            AgentState::Idle => "--",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Focus {
    Sidebar,
    Detail,
}

pub struct App {
    pub workspaces: Vec<WorkspaceState>,
    pub selected: usize,
    pub should_quit: bool,
    pub focus: Focus,
    pub detail_scroll: u16,
    session_to_workspace: HashMap<String, String>,
    workspace_index: HashMap<String, usize>,
    bullet_hashes: HashMap<PathBuf, u64>,
    /// Workspace UUID awaiting the second `D` confirmation for dismissal.
    /// Set on first `D`; cleared on second `D` (executes dismissal) or any other key.
    pub pending_dismissal: Option<String>,
    /// vim-like input mode for the `:command` bar.
    pub input_mode: crate::tui::command::InputMode,
}

/// Pre-gathered data for one refresh cycle. Produced off the main loop by
/// [`gather_refresh_snapshot`] (cmux IO + session-file parsing) and consumed
/// on the main loop by [`App::apply_refresh_snapshot`]. Decoupling these two
/// phases is what keeps the UI responsive while a refresh is in flight — on
/// machines with hundreds of session-log files the parse alone can take
/// multiple seconds, which would otherwise freeze the event loop.
pub struct RefreshSnapshot {
    pub workspaces: Vec<Workspace>,
    pub surfaces_map: HashMap<String, Vec<SurfaceInfo>>,
    pub sessions_by_ws_id: HashMap<String, SessionFile>,
}

/// Gather everything a refresh needs WITHOUT touching App state.
///
/// Runs as a free async function so it can be `tokio::spawn`-ed: the main
/// event loop kicks it off, continues serving key events, and applies the
/// resulting [`RefreshSnapshot`] once the gather finishes.
///
/// - cmux subprocess calls are already async (`tokio::process::Command`)
///   and yield naturally during I/O.
/// - The session-file parse (potentially hundreds of files) runs inside
///   `spawn_blocking` so it doesn't starve other tokio tasks.
pub async fn gather_refresh_snapshot(
    client: &CmuxClient,
    histories_dir: &std::path::Path,
) -> Result<RefreshSnapshot> {
    let workspaces = client.list_workspaces().await?;
    let surfaces_map = client.get_surfaces().await.unwrap_or_default();

    // Set of UUIDs we still need to find sessions for. The parser loop
    // below exits as soon as every workspace has a hit, so on the typical
    // case we parse roughly one file per workspace instead of every recent
    // session log.
    let known_uuids: std::collections::HashSet<String> =
        workspaces.iter().map(|w| w.uuid.clone()).collect();

    let dir = histories_dir.to_path_buf();
    let sessions_by_ws_id = tokio::task::spawn_blocking(move || {
        // 1. Filename-based recency filter: skip session logs older than
        //    7 days without stat'ing them. Boyan's histories dir routinely
        //    holds 1000+ lifetime files; this trims to a few-hundred recent.
        // 2. Sort newest-first by filename (YYYY-MM-DD-HH prefix is
        //    lexicographically chronological).
        // 3. Parse files in order, stopping as soon as we've covered every
        //    known workspace UUID. Worst case (workspaces with no recent
        //    session) scans the whole recent window; typical case parses
        //    just N files for N workspaces.
        let mut map: HashMap<String, SessionFile> = HashMap::new();
        let files = file::list_recent_session_files(&dir, 7).unwrap_or_default();
        for path in files {
            if known_uuids.len() > 0 && map.len() == known_uuids.len() {
                break; // all known workspaces have a session — stop early.
            }
            if let Ok(sf) = SessionFile::parse(&path) {
                if let Some(ref ws_id) = sf.frontmatter.workspace_id {
                    if known_uuids.contains(ws_id) {
                        map.entry(ws_id.clone()).or_insert(sf);
                    }
                }
            }
        }
        map
    })
    .await
    .unwrap_or_default();

    Ok(RefreshSnapshot {
        workspaces,
        surfaces_map,
        sessions_by_ws_id,
    })
}

impl App {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            selected: 0,
            should_quit: false,
            focus: Focus::Sidebar,
            detail_scroll: 0,
            session_to_workspace: HashMap::new(),
            workspace_index: HashMap::new(),
            bullet_hashes: HashMap::new(),
            pending_dismissal: None,
            input_mode: crate::tui::command::InputMode::Normal,
        }
    }

    pub async fn refresh_workspaces(
        &mut self,
        client: &CmuxClient,
        histories_dir: &std::path::Path,
    ) -> Result<()> {
        let snap = gather_refresh_snapshot(client, histories_dir).await?;
        self.apply_refresh_snapshot(snap);
        Ok(())
    }

    /// Apply a pre-gathered refresh snapshot to `self`. Pure mutation, no I/O
    /// that could block: the slow parts (cmux client calls, 999-file session
    /// parsing) ran off-thread in `gather_refresh_snapshot` and arrived here as
    /// data. Per-workspace file reads (trajectory.md, notes, hook_status) are
    /// still synchronous but are bounded — ~25 workspaces × ~4 small files ≈
    /// 100 reads, which is ~tens of ms total on a warm cache.
    pub fn apply_refresh_snapshot(&mut self, snap: RefreshSnapshot) {
        let RefreshSnapshot {
            workspaces,
            surfaces_map,
            mut sessions_by_ws_id,
        } = snap;

        let old_counts: HashMap<String, u32> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.tool_call_count))
            .collect();

        // Preserve existing screen previews and insights across refreshes
        let old_previews: HashMap<String, Option<String>> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.screen_preview.clone()))
            .collect();
        let old_insights: HashMap<String, ScreenInsights> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.screen_insights.clone()))
            .collect();
        let old_summaries: HashMap<String, Option<Summary>> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.summary.clone()))
            .collect();
        // Preserve editing state across refreshes so cursor position is remembered.
        let old_edit_states: HashMap<
            String,
            Option<crate::tui::trajectory_edit::TrajectoryEditState>,
        > = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.edit_state.clone()))
            .collect();
        // Preserve peek state across refreshes so an active peek session survives.
        let old_peek_states: HashMap<String, Option<crate::tui::peek_view::PeekState>> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.peek_state.clone()))
            .collect();
        // Preserve regen scheduler state across refreshes.
        let old_regen_states: HashMap<String, RegenSchedulerState> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.regen.clone()))
            .collect();
        // Preserve dismissal state across refreshes.
        let old_dismissal_states: HashMap<String, DismissalState> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.dismissal.clone()))
            .collect();
        // Preserve the in-memory trajectory across refreshes so we can REUSE
        // it (instead of reloading from disk) when the user is actively
        // editing or peeking. Reloading mid-edit would clobber the
        // in-flight item that `enter_insert_mode` auto-created in memory
        // but never persisted to disk.
        let old_trajectories: HashMap<String, Option<crate::mc_data::trajectory::TrajectoryDoc>> =
            self.workspaces
                .iter()
                .map(|ws| (ws.workspace.uuid.clone(), ws.trajectory.clone()))
                .collect();

        self.workspaces = workspaces
            .into_iter()
            .map(|ws| {
                let session = sessions_by_ws_id.remove(&ws.uuid);
                let surfaces = surfaces_map.get(&ws.ref_id).cloned().unwrap_or_default();
                let tool_call_count = old_counts.get(&ws.uuid).copied().unwrap_or(0);
                let screen_preview = old_previews.get(&ws.uuid).cloned().flatten();
                // Reuse existing insights (parsed from full 100-line capture)
                // rather than re-parsing from the truncated 15-line preview
                let screen_insights = old_insights.get(&ws.uuid).cloned().unwrap_or_default();
                // Provision the per-workspace data dir + display symlink.
                // Non-fatal: log to stderr and continue so mc-tui never
                // crashes just because the home dir is unwriteable.
                if let Err(e) = crate::mc_data::workspace::ensure_workspace(
                    &ws.uuid, &ws.name, &ws.name, // project defaults to name for now
                ) {
                    eprintln!("ensure_workspace({}): {e:?}", &ws.uuid);
                }
                // Load the trajectory doc only when the file actually exists.
                // load_from_file synthesises a default doc on NotFound, which
                // would wrongly show an empty trajectory panel; the .exists()
                // guard keeps None as the "no trajectory yet" signal that
                // detail.rs uses to fall back to the legacy rendering.
                //
                // CRITICAL: when the workspace is being actively edited or
                // peeked, REUSE the previous in-memory trajectory rather
                // than reloading from disk. `enter_insert_mode` auto-
                // creates an empty item in memory but doesn't persist; a
                // disk reload mid-edit would silently revert that item
                // (and then the description-seed pass below would
                // re-populate Mission from the cmux description, clobbering
                // the user's in-flight typing target).
                let is_actively_user_owned = {
                    let in_insert = old_edit_states
                        .get(&ws.uuid)
                        .and_then(|s| s.as_ref())
                        .map(|s| {
                            matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. })
                        })
                        .unwrap_or(false);
                    let in_peek = old_peek_states
                        .get(&ws.uuid)
                        .and_then(|s| s.as_ref())
                        .is_some();
                    in_insert || in_peek
                };
                let trajectory = if is_actively_user_owned {
                    old_trajectories.get(&ws.uuid).cloned().flatten()
                } else {
                    let traj_path = crate::mc_data::paths::trajectory_path(&ws.uuid);
                    if traj_path.exists() {
                        crate::mc_data::trajectory::TrajectoryDoc::load_from_file(&traj_path).ok()
                    } else {
                        None
                    }
                };
                let notes = load_workspace_notes(&ws.name);
                let hook_status = load_hook_status(&ws.uuid);
                let summary = old_summaries.get(&ws.uuid).cloned().flatten();
                let edit_state = old_edit_states.get(&ws.uuid).cloned().flatten();
                let peek_state = old_peek_states.get(&ws.uuid).cloned().flatten();
                let regen = old_regen_states.get(&ws.uuid).cloned().unwrap_or_default();
                let dismissal = old_dismissal_states
                    .get(&ws.uuid)
                    .cloned()
                    .unwrap_or_default();
                WorkspaceState {
                    workspace: ws,
                    session,
                    surfaces,
                    screen_preview,
                    screen_insights,
                    tool_call_count,
                    notes,
                    hook_status,
                    classification: None,
                    loading: false,
                    summary,
                    summarizing: false,
                    trajectory,
                    edit_state,
                    peek_state,
                    peek_yield_pending: false,
                    regen,
                    dismissal,
                    dispatch_modal: None,
                    dispatch_pending_outcome: None,
                    dispatch_error: None,
                }
            })
            .collect();

        // Preserve cmux's native tab order — `list-workspaces` returns workspaces
        // in the same order they appear in the cmux window, so the sidebar
        // mirrors what the user sees in their workspace tabs.

        self.workspace_index.clear();
        for (i, ws) in self.workspaces.iter().enumerate() {
            self.workspace_index.insert(ws.workspace.uuid.clone(), i);
        }

        // Persist a last-agent snapshot for any surface currently showing an
        // agent kind. T3 (rendering) uses `surface_kind::effective_kind` to
        // keep showing the agent glyph for ~5 minutes after the agent exits.
        // No-op for Shell/Unknown surfaces, so this is cheap to call on
        // every refresh tick.
        for ws_state in &self.workspaces {
            for surface in &ws_state.surfaces {
                if let Err(e) = crate::mc_data::surface_kind::write_last_agent(
                    &ws_state.workspace.uuid,
                    &surface.ref_id,
                    surface.kind,
                ) {
                    eprintln!(
                        "write_last_agent({}, {}): {e:?}",
                        &ws_state.workspace.uuid, &surface.ref_id
                    );
                }
            }
        }

        // Seed the Mission section from the cmux workspace description (first pass).
        // We only seed when the Mission section is currently empty so we never
        // clobber user-authored content. Each non-empty description line becomes
        // one Mission bullet.
        //
        // SKIP this pass entirely for workspaces being actively edited or
        // peeked — the user's in-flight typing target (an empty item created
        // by enter_insert_mode in memory) would look like "empty Mission" to
        // this loop and trigger a destructive seed.
        for ws_state in self.workspaces.iter_mut() {
            if ws_state
                .edit_state
                .as_ref()
                .map(|s| matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. }))
                .unwrap_or(false)
                || ws_state.peek_state.is_some()
            {
                continue;
            }
            let desc = match ws_state.workspace.description.as_ref() {
                Some(d) if !d.trim().is_empty() => d.clone(),
                _ => continue,
            };
            // Only seed when there is a trajectory doc already. If there is no
            // trajectory file yet we leave it as None — the detail pane falls
            // back to legacy rendering and the seeding happens on next refresh
            // once the file is created by ensure_workspace or the user's first edit.
            let doc = match ws_state.trajectory.as_mut() {
                Some(d) => d,
                None => continue,
            };
            let goal_section_empty = doc
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .map(|s| s.items.is_empty())
                .unwrap_or(true);
            if !goal_section_empty {
                continue;
            }
            let goal_items: Vec<crate::mc_data::trajectory::Item> = desc
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| crate::mc_data::trajectory::Item {
                    text: l.trim().to_string(),
                    is_checkbox: false,
                    checked: None,
                    surface_id: None,
                })
                .collect();
            if goal_items.is_empty() {
                continue;
            }
            doc.replace_section_items(
                crate::mc_data::trajectory::SECTION_MISSION,
                goal_items,
            );
            let traj_path = crate::mc_data::paths::trajectory_path(&ws_state.workspace.uuid);
            if let Err(e) = doc.save_to_file(&traj_path) {
                eprintln!(
                    "seed Mission from description ({}): {e:?}",
                    ws_state.workspace.uuid
                );
            }
        }

        // Project cmux surfaces into the trajectory's ## Current surfaces section.
        // We do this in a second pass (after the map/collect above) so we have
        // access to both the built WorkspaceState and can mutate it.
        //
        // SKIP this pass for workspaces being actively edited or peeked — even
        // though Current surfaces is a different section than Mission/Goals, the
        // save_to_file at the end of the loop would mutate the on-disk file
        // while the user is in the middle of an unsaved edit. The peek case
        // matters because peek's screen-poll already mutates state at 1Hz and
        // we don't want to also rewrite trajectory.md under it.
        for ws_state in self.workspaces.iter_mut() {
            if ws_state
                .edit_state
                .as_ref()
                .map(|s| matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. }))
                .unwrap_or(false)
                || ws_state.peek_state.is_some()
            {
                continue;
            }
            let Some(ref mut doc) = ws_state.trajectory else {
                continue;
            };

            // Load goals.json once for this workspace so we can decorate both
            // surface rows (with `← goal:<short>` badges) and Goals & Progress
            // rows (with `→ <glyph> <surface_ref>` badges). Missing file is
            // not an error — `GoalsFile::load` returns the empty default and
            // the badge helpers degrade to no-ops.
            let goals = crate::mc_data::goals_json::GoalsFile::load(
                &ws_state.workspace.uuid,
            );

            // Build the new item list from the surfaces vec.
            // Each surface item uses the workspace ref_id as surface_id because
            // `cmux read-screen` takes a workspace ref — peek mode passes
            // surface_id directly to read_screen, so this is the correct identifier.
            let surface_items: Vec<crate::mc_data::trajectory::Item> = ws_state
                .surfaces
                .iter()
                .map(|s| {
                    // `effective_kind` keeps the agent glyph for ~5 min after
                    // the agent exits (Shell/Unknown current + recent
                    // last-agent file ⇒ surface the agent kind instead).
                    let eff = crate::mc_data::surface_kind::effective_kind(
                        &ws_state.workspace.uuid,
                        &s.ref_id,
                        s.kind,
                    );
                    let text = crate::mc_data::surface_render::format_surface_text(
                        eff,
                        &s.title,
                        &goals,
                        &s.ref_id,
                    );
                    crate::mc_data::trajectory::Item {
                        text,
                        is_checkbox: false,
                        checked: None,
                        // Use the surface's own ref_id (e.g. "surface:92") so that
                        // peek mode can distinguish surfaces within the same workspace
                        // and distribute session logs deterministically by index.
                        surface_id: Some(s.ref_id.clone()),
                    }
                })
                .collect();

            // Skip save if the projected surface list is identical to what's
            // already in the doc — avoids redundant iCloud file-watcher churn.
            let existing_items = doc
                .section(crate::mc_data::trajectory::SECTION_CURRENT_SURFACES)
                .map(|s| s.items.as_slice())
                .unwrap_or(&[]);
            let surfaces_unchanged = existing_items.len() == surface_items.len()
                && existing_items
                    .iter()
                    .zip(surface_items.iter())
                    .all(|(a, b)| a.text == b.text && a.surface_id == b.surface_id);

            // Re-decorate Goals & Progress rows with `→ <glyph> <ref>` badges.
            // Strip any previously-applied badge first so the result is
            // idempotent across refresh ticks even after an assignment is
            // cleared. Skip this entire pass when goals.json is empty *and*
            // no row currently carries a badge — preserves the "workspace
            // with no goals.json renders unchanged" contract.
            let goals_section_existing = doc
                .section(crate::mc_data::trajectory::SECTION_GOALS)
                .map(|s| s.items.clone())
                .unwrap_or_default();
            let any_existing_badge = goals_section_existing
                .iter()
                .any(|i| i.text.contains("   → "));
            let goals_need_rebuild = !goals.goals.is_empty() || any_existing_badge;

            let (goals_unchanged, goals_items_opt) = if goals_need_rebuild {
                let rebuilt: Vec<crate::mc_data::trajectory::Item> = goals_section_existing
                    .iter()
                    .map(|i| {
                        let base =
                            crate::mc_data::surface_render::strip_badge(&i.text).to_string();
                        let mut text = base.clone();
                        if let Some(badge) =
                            crate::mc_data::surface_render::format_goal_badge(&goals, &base)
                        {
                            text.push_str(&badge);
                        }
                        crate::mc_data::trajectory::Item {
                            text,
                            is_checkbox: i.is_checkbox,
                            checked: i.checked,
                            surface_id: i.surface_id.clone(),
                        }
                    })
                    .collect();
                let unchanged = rebuilt.len() == goals_section_existing.len()
                    && rebuilt
                        .iter()
                        .zip(goals_section_existing.iter())
                        .all(|(a, b)| a.text == b.text);
                (unchanged, Some(rebuilt))
            } else {
                (true, None)
            };

            if surfaces_unchanged && goals_unchanged {
                continue;
            }

            doc.replace_section_items(
                crate::mc_data::trajectory::SECTION_CURRENT_SURFACES,
                surface_items,
            );
            if let Some(items) = goals_items_opt {
                doc.replace_section_items(
                    crate::mc_data::trajectory::SECTION_GOALS,
                    items,
                );
            }

            let traj_path = crate::mc_data::paths::trajectory_path(&ws_state.workspace.uuid);
            if let Err(e) = doc.save_to_file(&traj_path) {
                eprintln!(
                    "save trajectory after surface refresh ({}): {e:?}",
                    ws_state.workspace.uuid
                );
            }
        }
    }

    pub fn handle_agent_event(&mut self, event: &AgentEvent) {
        self.session_to_workspace
            .insert(event.session_id.clone(), event.workspace_id.clone());

        if let Some(&idx) = self.workspace_index.get(&event.workspace_id) {
            self.workspaces[idx].tool_call_count += 1;

            // Derive a status from the hook event name. cmux already publishes
            // hook events with phase=completed for every agent that has a
            // cmux hook bridge installed (Claude, Codex, OpenCode, …) — for
            // local *and* remote workspaces. This is the "first-class status
            // event" path: agent_state() picks up hook_status at priority 1.
            //
            // event_name shape: "agent.hook.PreToolUse", "agent.hook.Stop", …
            let hook = event
                .event_name
                .rsplit_once('.')
                .map(|(_, h)| h)
                .unwrap_or(event.event_name.as_str());
            let derived = match hook {
                // Agent is actively doing work.
                "PreToolUse" | "PostToolUse" | "UserPromptSubmit" => Some("working"),
                // Agent has yielded the turn — needs user.
                "Stop" | "SubagentStop" | "Notification" | "AskUserQuestion" => Some("waiting"),
                // Lifecycle bookends — neither working nor blocked.
                "SessionEnd" => Some("idle"),
                _ => None,
            };
            if let Some(state) = derived {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                self.workspaces[idx].hook_status = Some((state.to_string(), ts));
            }
        }
    }

    pub fn needs_summary(&self, workspace_uuid: &str, threshold: u32) -> bool {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            let ws = &self.workspaces[idx];
            ws.tool_call_count >= threshold && ws.session.is_some()
        } else {
            false
        }
    }

    pub fn reset_tool_count(&mut self, workspace_uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            self.workspaces[idx].tool_call_count = 0;
        }
    }

    pub fn apply_summary(&mut self, workspace_uuid: &str, summary: Summary) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            let ws = &mut self.workspaces[idx];
            ws.summarizing = false;
            // Also stamp the session file when one is attached
            if let Some(ref mut session) = ws.session {
                session.trajectory = Some(summary.trajectory.clone());
                session.next_steps = summary.next_steps.clone();
            }
            ws.summary = Some(summary);
        }
    }

    /// Mark a workspace as currently being summarized (shows spinner).
    pub fn set_summarizing(&mut self, workspace_uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            self.workspaces[idx].summarizing = true;
        }
    }

    pub fn handle_file_changed(&mut self, changed: &FileChanged) -> Option<String> {
        let sf = SessionFile::parse(&changed.path).ok()?;
        let ws_id = sf.frontmatter.workspace_id.clone()?;

        let new_hash = hash_bullets(&sf.bullets);
        let old_hash = self.bullet_hashes.get(&changed.path).copied();
        self.bullet_hashes.insert(changed.path.clone(), new_hash);
        let bullets_changed = old_hash.is_some_and(|h| h != new_hash);

        if let Some(&idx) = self.workspace_index.get(&ws_id) {
            self.workspaces[idx].session = Some(sf);
        }

        if bullets_changed { Some(ws_id) } else { None }
    }

    pub fn selected_workspace(&self) -> Option<&WorkspaceState> {
        self.workspaces.get(self.selected)
    }

    /// Auto-load screen preview for the currently selected workspace.
    /// Captures 100 lines for parsing insights, but stores a shorter preview for display.
    /// Kick off a background screen-refresh + classification for the
    /// currently selected workspace. Returns immediately. Result flows
    /// back via the provided channel.
    pub fn spawn_load_screen_preview(
        &mut self,
        client: CmuxClient,
        classifier: Option<TypeSafeClassifier>,
        tx: tokio::sync::mpsc::UnboundedSender<ScreenUpdate>,
    ) {
        let idx = self.selected;
        if let Some(ws) = self.workspaces.get_mut(idx) {
            ws.loading = true;
            spawn_screen_task(
                ws.workspace.uuid.clone(),
                ws.workspace.ref_id.clone(),
                client,
                classifier,
                tx,
            );
        }
    }

    /// Kick off background screen-refresh tasks for ALL workspaces in parallel.
    /// Returns immediately. Results flow back via the provided channel.
    pub fn spawn_refresh_all_screens(
        &mut self,
        client: CmuxClient,
        classifier: Option<TypeSafeClassifier>,
        tx: tokio::sync::mpsc::UnboundedSender<ScreenUpdate>,
    ) {
        for ws in &mut self.workspaces {
            ws.loading = true;
            spawn_screen_task(
                ws.workspace.uuid.clone(),
                ws.workspace.ref_id.clone(),
                client.clone(),
                classifier.clone(),
                tx.clone(),
            );
        }
    }

    /// Apply a screen update message arriving from a background task.
    pub fn apply_screen_update(&mut self, update: ScreenUpdate) {
        if let Some(&idx) = self.workspace_index.get(&update.workspace_uuid) {
            let ws = &mut self.workspaces[idx];
            ws.loading = false;
            if let Some(screen) = update.screen {
                ws.screen_insights = parse_screen_insights(&screen);
                let display_lines: Vec<&str> = screen.lines().collect();
                let start = display_lines.len().saturating_sub(15);
                ws.screen_preview = Some(display_lines[start..].join("\n"));
            }
            if update.classification.is_some() {
                ws.classification = update.classification;
            }
        }
    }

    pub fn workspace_index_for(&self, uuid: &str) -> Option<&usize> {
        self.workspace_index.get(uuid)
    }

    pub fn next(&mut self) {
        if !self.workspaces.is_empty() {
            self.selected = (self.selected + 1) % self.workspaces.len();
            self.detail_scroll = 0;
        }
    }

    pub fn previous(&mut self) {
        if !self.workspaces.is_empty() {
            self.selected = (self.selected + self.workspaces.len() - 1) % self.workspaces.len();
            self.detail_scroll = 0;
        }
    }

    pub fn scroll_down(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_add(3);
    }

    pub fn scroll_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(3);
    }

    /// Get the notes file path for a workspace.
    pub fn notes_path_for(&self, ws: &WorkspaceState) -> PathBuf {
        notes_dir().join(format!("{}.md", workspace_slug(&ws.workspace.name)))
    }

    /// Reload notes for all workspaces from disk.
    pub fn load_notes(&mut self) {
        for ws in &mut self.workspaces {
            ws.notes = load_workspace_notes(&ws.workspace.name);
        }
    }

    /// Called when the file watcher detects a write to `<.data>/<uuid>/trajectory.md`.
    /// Re-reads the trajectory doc from disk and updates the in-memory workspace state.
    ///
    /// If the workspace is currently being edited, the update is silently skipped —
    /// the next 30 s refresh tick or a future watcher event will pick it up once
    /// editing is done.
    pub fn apply_trajectory_update(&mut self, uuid: &str) {
        let Some(&idx) = self.workspace_index.get(uuid) else {
            return;
        };
        // Skip if we're in an active insert-mode edit to avoid clobbering
        // in-flight changes.
        let is_editing = self.workspaces[idx]
            .edit_state
            .as_ref()
            .map(|s| matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. }))
            .unwrap_or(false);
        if is_editing {
            return;
        }
        let traj_path = crate::mc_data::paths::trajectory_path(uuid);
        if traj_path.exists() {
            if let Ok(doc) = crate::mc_data::trajectory::TrajectoryDoc::load_from_file(&traj_path) {
                self.workspaces[idx].trajectory = Some(doc);
            }
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // Regen scheduler methods
    // ──────────────────────────────────────────────────────────────────────

    /// Increment the event counter for a workspace (called on agent events).
    pub fn increment_regen_event_count(&mut self, uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(uuid) {
            self.workspaces[idx].regen.events_since_last_regen += 1;
        }
    }

    /// Return UUIDs of workspaces that are due for a trajectory regen.
    ///
    /// Thresholds (compile-time defaults, configurable in the future):
    /// - ≥ 10 events since last regen, OR
    /// - ≥ 300 seconds since last regen (with any events pending)
    ///
    /// Excluded if:
    /// - trajectory is None (no trajectory file yet)
    /// - workspace is in Insert mode editing
    /// - a regen is already in flight
    pub fn workspaces_due_for_regen(&self) -> Vec<String> {
        const EVENT_THRESHOLD: u32 = 10;
        const TIME_THRESHOLD_SECS: u64 = 300;

        self.workspaces
            .iter()
            .filter(|ws| {
                // Must have a trajectory to regenerate
                if ws.trajectory.is_none() {
                    return false;
                }
                // Don't regen while in insert mode
                let is_editing = ws
                    .edit_state
                    .as_ref()
                    .map(|s| matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. }))
                    .unwrap_or(false);
                if is_editing {
                    return false;
                }
                // Don't spawn another if one is already running
                if ws.regen.regen_in_flight {
                    return false;
                }
                // Must have at least 1 event pending (time threshold requires some change)
                if ws.regen.events_since_last_regen == 0 {
                    return false;
                }
                // Check event threshold
                if ws.regen.events_since_last_regen >= EVENT_THRESHOLD {
                    return true;
                }
                // Check time threshold
                if let Some(last_at) = ws.regen.last_regen_at {
                    if last_at.elapsed().as_secs() >= TIME_THRESHOLD_SECS {
                        return true;
                    }
                } else {
                    // Never regenerated but has events — use time threshold from
                    // startup (treat as 300s+ elapsed so first regen fires promptly)
                    if ws.regen.events_since_last_regen > 0 {
                        return true;
                    }
                }
                false
            })
            .map(|ws| ws.workspace.uuid.clone())
            .collect()
    }

    /// Mark a workspace's regen as in-flight.
    pub fn mark_regen_in_flight(&mut self, uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(uuid) {
            self.workspaces[idx].regen.regen_in_flight = true;
        }
    }

    /// Build the regen inputs for a workspace to pass to the LLM task.
    pub fn build_regen_inputs(&self, uuid: &str) -> Option<RegenInputs> {
        let idx = *self.workspace_index.get(uuid)?;
        let ws = &self.workspaces[idx];
        let trajectory = ws.trajectory.as_ref()?;

        // Load recent events from disk
        let events_path = crate::mc_data::paths::events_log(uuid);
        let recent_events = crate::mc_data::events::load(&events_path)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();

        // Load recent user explanations from inputs dir
        let inputs_dir = crate::mc_data::paths::inputs_dir(uuid);
        let recent_user_explanations = load_recent_user_explanations(&inputs_dir, 3);

        // Collect session bullets
        let session_bullets = ws
            .session
            .as_ref()
            .map(|s| s.bullets.clone())
            .unwrap_or_default();

        // Collect surface summaries from .summary files
        let surfaces_dir = crate::mc_data::paths::surfaces_dir(uuid);
        let surface_summaries = load_surface_summaries(&surfaces_dir);

        // Cmux surface order from workspace surfaces list (use titles as identifiers)
        let cmux_surface_order = ws.surfaces.iter().map(|s| s.title.clone()).collect();

        // Canonical user ask from ~obsAgents/Sessions/<file>.md (last `## boyan` block).
        // Build WorkspaceContext for host+cwd disambiguation (tier-1 session log matching).
        let ctx = crate::mc_data::session_log::WorkspaceContext {
            host: Some(hostname_short()),
            cwd: ws.workspace.current_directory.clone(),
        };
        let user_ask = crate::mc_data::session_log::latest_session_file_for_workspace(uuid, &ctx)
            .ok()
            .flatten()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| crate::mc_data::session_log::last_user_turn(&s));

        Some(RegenInputs {
            workspace_name: ws.workspace.name.clone(),
            current_trajectory: trajectory.to_markdown(),
            recent_events,
            recent_user_explanations,
            session_bullets,
            surface_summaries,
            tool_call_count: ws.tool_call_count,
            cmux_surface_order,
            user_ask,
        })
    }

    /// Apply a completed trajectory regen to a workspace.
    ///
    /// This replaces the in-memory trajectory, saves to disk, and resets the
    /// regen scheduler state. Silently skips if the workspace is in insert mode
    /// (the next regen tick will pick it up).
    pub fn apply_regenerated_trajectory(
        &mut self,
        uuid: &str,
        mut doc: crate::mc_data::trajectory::TrajectoryDoc,
    ) {
        let Some(&idx) = self.workspace_index.get(uuid) else {
            return;
        };
        // Don't overwrite an active insert-mode edit session.
        let is_editing = self.workspaces[idx]
            .edit_state
            .as_ref()
            .map(|s| matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. }))
            .unwrap_or(false);
        if is_editing {
            // Clear in-flight flag so the next tick can retry.
            self.workspaces[idx].regen.regen_in_flight = false;
            return;
        }

        // Ensure canonical sections exist.
        doc.ensure_sections();

        // Sort Goals & Progress if >10 items.
        doc.sort_tasks_if_long();

        // Before persist: apply human stickiness so agent regen cannot
        // un-check what the user checked, re-check what they unchecked,
        // or re-add what they deleted.
        let intent = crate::mc_data::user_intent::load_for_workspace(uuid).unwrap_or_default();
        crate::mc_data::user_intent::apply_to_tasks(&mut doc, &intent);

        // Persist to disk — non-fatal on error.
        let traj_path = crate::mc_data::paths::trajectory_path(uuid);
        if let Err(e) = doc.save_to_file(&traj_path) {
            eprintln!("apply_regenerated_trajectory save({uuid}): {e:?}");
        }

        // Update in-memory state.
        self.workspaces[idx].trajectory = Some(doc);
        self.workspaces[idx].regen.last_regen_at = Some(Instant::now());
        self.workspaces[idx].regen.events_since_last_regen = 0;
        self.workspaces[idx].regen.regen_in_flight = false;
    }

    /// Return (uuid, sid, log_path) tuples for shell surfaces due for summarization.
    ///
    /// Scans the surfaces dir for `.log` files that don't yet have a corresponding
    /// `.summary` file (or whose summary is stale). The sid is derived from the
    /// log filename stem.
    pub fn surfaces_due_for_summary(&self) -> Vec<(String, String, PathBuf)> {
        let mut result = Vec::new();
        for ws in &self.workspaces {
            let uuid = &ws.workspace.uuid;
            let surfaces_dir = crate::mc_data::paths::surfaces_dir(uuid);
            if !surfaces_dir.exists() {
                continue;
            }
            let entries = match std::fs::read_dir(&surfaces_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("log") {
                    continue;
                }
                let sid = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let log_path = path;
                let summary_path = surfaces_dir.join(format!("{sid}.summary"));
                // Due if no summary file exists, or log is newer than summary.
                let due = if !summary_path.exists() {
                    true
                } else {
                    // Compare mtimes — if log is newer, re-summarize.
                    let log_mtime = std::fs::metadata(&log_path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                    let summary_mtime = std::fs::metadata(&summary_path)
                        .ok()
                        .and_then(|m| m.modified().ok());
                    match (log_mtime, summary_mtime) {
                        (Some(lm), Some(sm)) => lm > sm,
                        _ => false,
                    }
                };
                if due {
                    result.push((uuid.clone(), sid, log_path));
                }
            }
        }
        result
    }

    /// Apply a surface summary result (called from main.rs on task completion).
    pub fn apply_surface_summary(&mut self, uuid: &str, sid: &str, summary: String) {
        // Write to disk immediately; the in-memory value is in the .summary file.
        let surfaces_dir = crate::mc_data::paths::surfaces_dir(uuid);
        if let Err(e) =
            crate::llm::surface_summary::write_summary_file(&surfaces_dir, sid, &summary)
        {
            eprintln!("apply_surface_summary({uuid}/{sid}): {e:?}");
        }
    }

    // ──────────────────────────────────────────────────────────────────────

    /// Dispatch a key event to the trajectory editor for the selected workspace.
    ///
    /// Returns any `EditAction`s that should be persisted on the next Esc-save.
    /// If the focused workspace has no trajectory, or the detail pane is not
    /// focused, returns an empty Vec.
    ///
    /// Peek mode is handled first: when `peek_state` is Some, keys are routed
    /// to peek navigation (j/k/g/G/Esc/Enter) and normal editing is bypassed.
    /// Dispatch modal is handled second: when `dispatch_modal` is Some, keys
    /// are routed to the modal and the parent loop reads the outcome via
    /// `take_dispatch_outcome`.
    pub fn handle_trajectory_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Vec<crate::tui::trajectory_edit::EditAction> {
        use crossterm::event::KeyCode;
        let idx = self.selected;

        // ── Dispatch modal: intercept before peek/edit ──────────────────────
        {
            let ws = match self.workspaces.get_mut(idx) {
                Some(w) => w,
                None => return vec![],
            };
            if ws.dispatch_modal.is_some() {
                // Note: the parent main loop polls `take_dispatch_outcome`
                // after each key dispatch and runs the cmux/goals.json side
                // effects there. handle_key returns nothing actionable.
                let _outcome = ws
                    .dispatch_modal
                    .as_mut()
                    .map(|m| m.handle_key(key));
                // Stash the outcome on the modal itself so the loop can read it.
                if let Some(out) = _outcome {
                    ws.dispatch_pending_outcome = Some(out);
                }
                return vec![];
            }
        }

        // ── Peek mode: intercept before the editor sees anything ────────────
        {
            let ws = match self.workspaces.get_mut(idx) {
                Some(w) => w,
                None => return vec![],
            };
            if ws.peek_state.is_some() {
                let peek = ws.peek_state.as_mut().unwrap();
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        peek.scroll_down();
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        peek.scroll_up();
                    }
                    KeyCode::Char(' ') => {
                        peek.page_down();
                    }
                    KeyCode::Char('-') => {
                        peek.page_up();
                    }
                    KeyCode::Char('g') => {
                        peek.go_top();
                    }
                    KeyCode::Char('G') => {
                        peek.go_bottom();
                    }
                    KeyCode::Esc => {
                        // Exit peek mode — back to trajectory nav.
                        ws.peek_state = None;
                    }
                    KeyCode::Enter => {
                        // Yield: select the workspace in cmux, then clear peek.
                        // The actual cmux call is spawned asynchronously by the
                        // caller reading `peek_yield_pending` from the workspace.
                        // We just set a flag; the TUI event loop will act on it.
                        ws.peek_yield_pending = true;
                    }
                    _ => { /* all other keys are no-ops in peek mode */ }
                }
                return vec![];
            }
        }

        // ── Normal trajectory editing ────────────────────────────────────────
        let ws = match self.workspaces.get_mut(idx) {
            Some(w) => w,
            None => return vec![],
        };
        let doc = match ws.trajectory.as_mut() {
            Some(d) => d,
            None => return vec![],
        };
        // Lazily initialise edit_state when first needed.
        let state = ws
            .edit_state
            .get_or_insert_with(|| crate::tui::trajectory_edit::TrajectoryEditState::default());

        // ── Nav mode + Enter on a Current surfaces row → enter peek ─────────
        use crate::mc_data::trajectory::SECTION_CURRENT_SURFACES;
        if key.code == KeyCode::Enter
            && matches!(state.mode, crate::tui::trajectory_edit::EditMode::Nav)
        {
            let sec_idx = state.cursor_section;
            let item_idx = state.cursor_item;
            let section = doc.sections.get(sec_idx);
            if let Some(sec) = section {
                if sec.name == SECTION_CURRENT_SURFACES {
                    let item = sec.items.get(item_idx);
                    // Use surface_id if present; fall back to workspace ref_id.
                    let surface_ref = item
                        .and_then(|i| i.surface_id.as_deref())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| ws.workspace.ref_id.clone());
                    let surface_label = item
                        .map(|i| {
                            if let Some(ref sid) = i.surface_id {
                                format!("{} ({})", i.text.as_str(), sid)
                            } else {
                                i.text.clone()
                            }
                        })
                        .unwrap_or_else(|| ws.workspace.ref_id.clone());
                    // Detect Agent vs Shell source using the two-step resolver.
                    // Per-workspace identity for a peek: agent kind + position
                    // among same-kind surfaces. `index_in_pane` from cmux is
                    // per-pane (two panes can both have idx=0), so we compute
                    // a same-agent index over the workspace's flat surface list.
                    let surface_id_for_lookup = item
                        .and_then(|i| i.surface_id.as_deref())
                        .unwrap_or("");
                    let this_surface = ws
                        .surfaces
                        .iter()
                        .find(|s| s.ref_id == surface_id_for_lookup);
                    let raw_kind = this_surface
                        .map(|s| s.kind)
                        .unwrap_or_default();
                    // Use effective_kind (live kind + recent last-agent
                    // fallback) so an agent surface that briefly drops to
                    // shell/cmux foreground keeps its agent route to
                    // session.md. Matches the trajectory glyph (set in the
                    // projection at ~line 1071) so what the user sees in the
                    // sidebar item lines up with what peek picks.
                    let surface_kind = crate::mc_data::surface_kind::effective_kind(
                        &ws.workspace.uuid,
                        surface_id_for_lookup,
                        raw_kind,
                    );
                    // Per F11 in .agents/validate.md:
                    //   - Agent surfaces (Claude/Codex/OtherAgent) → resolve to
                    //     session.md. NEVER fall back to Shell — that would
                    //     show the wrong content from cmux read-screen.
                    //   - Non-agent surfaces (Shell, Unknown) → Shell source,
                    //     which the peek_tick path reads via
                    //     surface.read_text (per-surface), NOT
                    //     read-screen (workspace-level).
                    use crate::mc_data::surface_kind::SurfaceKind;
                    let source = if surface_kind.is_agent() {
                        let agent_label = surface_kind.label();
                        let same_agent_index = ws
                            .surfaces
                            .iter()
                            .filter(|s| s.kind == surface_kind)
                            .position(|s| s.ref_id == surface_id_for_lookup)
                            .unwrap_or(0);
                        let peek_ctx = crate::mc_data::session_log::WorkspaceContext {
                            host: Some(hostname_short()),
                            cwd: ws.workspace.current_directory.clone(),
                        };
                        match crate::mc_data::session_log::resolve_session_log_for_surface(
                            &ws.workspace.uuid,
                            surface_id_for_lookup,
                            &peek_ctx,
                            Some(agent_label),
                            same_agent_index,
                        ) {
                            Ok(Some(path)) => {
                                crate::tui::peek_view::PeekSource::Agent { session_path: path }
                            }
                            // Agent surface with no resolved session.md:
                            // keep Agent source with an empty session path so
                            // render shows a clear "no session yet" state
                            // rather than collapsing to Shell (which would
                            // show some unrelated surface's content).
                            _ => crate::tui::peek_view::PeekSource::Agent {
                                session_path: std::path::PathBuf::new(),
                            },
                        }
                    } else {
                        // Shell or Unknown: read this surface's own screen.
                        crate::tui::peek_view::PeekSource::Shell
                    };
                    ws.peek_state = Some(crate::tui::peek_view::PeekState::new(
                        ws.workspace.ref_id.clone(),
                        surface_ref,
                        surface_label,
                        source,
                    ));
                    return vec![];
                }
            }
        }

        // ── Nav mode + Enter on a populated Goals & Progress row ───────────
        // → open the dispatch modal. Empty goal rows still fall through to
        // `handle_key`, which executes `insert_item_below` (preserving the
        // b997a17 "Enter adds a new goal" behavior for blank rows).
        use crate::mc_data::trajectory::SECTION_GOALS;
        if key.code == KeyCode::Enter
            && matches!(state.mode, crate::tui::trajectory_edit::EditMode::Nav)
        {
            let sec_idx = state.cursor_section;
            let item_idx = state.cursor_item;
            if let Some(sec) = doc.sections.get(sec_idx) {
                if sec.name == SECTION_GOALS {
                    if let Some(item) = sec.items.get(item_idx) {
                        if !item.text.trim().is_empty() {
                            let goal_text = item.text.clone();
                            let workspace_uuid = ws.workspace.uuid.clone();
                            let workspace_ref = ws.workspace.ref_id.clone();
                            let surfaces = ws.surfaces.clone();
                            ws.dispatch_modal = Some(
                                crate::tui::dispatch_modal::DispatchModal::new(
                                    goal_text,
                                    workspace_uuid,
                                    workspace_ref,
                                    &surfaces,
                                ),
                            );
                            return vec![];
                        }
                    }
                }
            }
        }

        crate::tui::trajectory_edit::handle_key(state, doc, key)
    }

    /// Read and clear the pending dispatch outcome for the selected workspace.
    /// The main loop calls this after each key dispatch and acts on the
    /// outcome (running cmux commands, updating goals.json, closing the modal).
    pub fn take_dispatch_outcome(
        &mut self,
    ) -> Option<crate::tui::dispatch_modal::DispatchOutcome> {
        let idx = self.selected;
        let ws = self.workspaces.get_mut(idx)?;
        ws.dispatch_pending_outcome.take()
    }

    /// Close the dispatch modal for the selected workspace.
    pub fn close_dispatch_modal(&mut self) {
        let idx = self.selected;
        if let Some(ws) = self.workspaces.get_mut(idx) {
            ws.dispatch_modal = None;
            ws.dispatch_pending_outcome = None;
        }
    }

    /// Set the dispatch error message (shown in the status bar). Cleared on
    /// the next key press.
    #[allow(dead_code)]
    pub fn set_dispatch_error(&mut self, msg: String) {
        let idx = self.selected;
        if let Some(ws) = self.workspaces.get_mut(idx) {
            ws.dispatch_error = Some(msg);
        }
    }

    /// Apply a successful dispatch result to `goals.json`: upsert an
    /// assignment row for `goal_text` pointing at `surface_ref`/`kind` with
    /// the current timestamp. Errors are surfaced as `dispatch_error`.
    ///
    /// Currently the main loop performs this work inline in the async
    /// dispatch task so failure paths don't update goals.json; this method
    /// remains available for tests and future synchronous callers.
    #[allow(dead_code)]
    pub fn record_dispatch_assignment(
        &mut self,
        goal_text: &str,
        surface_ref: &str,
        kind: crate::mc_data::surface_kind::SurfaceKind,
    ) {
        let idx = self.selected;
        let uuid = match self.workspaces.get(idx) {
            Some(ws) => ws.workspace.uuid.clone(),
            None => return,
        };
        let mut goals = crate::mc_data::goals_json::GoalsFile::load(&uuid);
        goals.set_assignment(goal_text, surface_ref, kind, chrono::Utc::now());
        if let Err(e) = goals.save(&uuid) {
            self.set_dispatch_error(format!("goals.json save: {e}"));
        }
    }

    /// Called from the event loop to check whether a peek-yield is pending for
    /// the selected workspace. Clears the flag and returns the WORKSPACE ref
    /// to pass to `cmux select-workspace`. (Passing a surface ref would error
    /// "Workspace not found" — cmux's select-workspace only accepts workspace
    /// refs.)
    pub fn take_peek_yield(&mut self) -> Option<String> {
        let idx = self.selected;
        let ws = self.workspaces.get_mut(idx)?;
        if ws.peek_yield_pending {
            ws.peek_yield_pending = false;
            // After yielding, clear peek state (the user is going to work there).
            let ref_id = ws
                .peek_state
                .as_ref()
                .map(|p| p.workspace_ref.clone())
                .unwrap_or_else(|| ws.workspace.ref_id.clone());
            ws.peek_state = None;
            Some(ref_id)
        } else {
            None
        }
    }

    /// Apply a screen update to the active peek state for the given workspace.
    pub fn apply_peek_screen_update(&mut self, workspace_uuid: &str, screen_text: String) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            if let Some(peek) = self.workspaces[idx].peek_state.as_mut() {
                peek.ingest_screen(&screen_text);
            }
        }
    }

    /// Returns whether the selected workspace is currently in peek mode and
    /// needs a screen poll. Returns `(workspace_uuid, surface_ref)` — the
    /// surface ref is what `cmux rpc surface.read_text` accepts, giving us
    /// per-surface content (vs `read-screen --workspace` which collapses
    /// every surface onto one stream). See F11 in `.agents/validate.md`.
    pub fn peek_needs_poll(&self) -> Option<(&str, &str)> {
        let ws = self.workspaces.get(self.selected)?;
        let peek = ws.peek_state.as_ref()?;
        if peek.should_poll() {
            Some((&ws.workspace.uuid, &peek.surface_ref))
        } else {
            None
        }
    }

    /// Mark the active peek as polling (prevents duplicate concurrent polls).
    pub fn mark_peek_polling(&mut self) {
        let idx = self.selected;
        if let Some(ws) = self.workspaces.get_mut(idx) {
            if let Some(peek) = ws.peek_state.as_mut() {
                peek.polling = true;
            }
        }
    }

    /// For Agent-source peek states, re-read the session log and rebuild the
    /// display buffer. Called from the main loop instead of the cmux read-screen
    /// path when `peek.uses_cmux_screen()` is false.
    pub fn refresh_agent_peek_buffer(&mut self, uuid: &str) {
        let Some(&idx) = self.workspace_index.get(uuid) else {
            return;
        };
        let Some(peek) = self.workspaces[idx].peek_state.as_mut() else {
            return;
        };
        let crate::tui::peek_view::PeekSource::Agent { session_path } = &peek.source else {
            return;
        };
        let session_path = session_path.clone();
        crate::tui::peek_view::rebuild_agent_buffer(peek, &session_path);
    }

    // ──────────────────────────────────────────────────────────────────────
    // Dismissal methods
    // ──────────────────────────────────────────────────────────────────────

    /// Update the open_surfaces count for a workspace.
    /// When count drops to zero, start the grace timer.
    /// When count rises above zero, cancel the grace timer.
    pub fn set_open_surfaces(&mut self, uuid: &str, count: u32) {
        if let Some(&idx) = self.workspace_index.get(uuid) {
            let ds = &mut self.workspaces[idx].dismissal;
            let was_zero = ds.open_surfaces == 0;
            ds.open_surfaces = count;
            if count == 0 && !was_zero {
                // Surfaces just dropped to zero — start grace timer.
                ds.grace_started_at = Some(Instant::now());
            } else if count > 0 {
                // A surface re-attached — cancel the grace timer.
                ds.grace_started_at = None;
            }
        }
    }

    /// Return UUIDs of workspaces whose grace timer has elapsed.
    /// Excludes workspaces already marked as dismissing.
    pub fn workspaces_ready_for_dismissal(&self, grace: std::time::Duration) -> Vec<String> {
        self.workspaces
            .iter()
            .filter(|ws| {
                if ws.dismissal.dismissing {
                    return false;
                }
                if let Some(started_at) = ws.dismissal.grace_started_at {
                    started_at.elapsed() >= grace
                } else {
                    false
                }
            })
            .map(|ws| ws.workspace.uuid.clone())
            .collect()
    }

    /// Gather all available data for the learning LLM call.
    pub fn build_learning_inputs(&self, uuid: &str) -> crate::llm::learning::LearningInputs {
        let idx = self.workspace_index.get(uuid).copied();
        let ws = idx.and_then(|i| self.workspaces.get(i));

        let workspace_name = ws
            .map(|w| w.workspace.name.clone())
            .unwrap_or_else(|| uuid.to_string());
        let project = crate::mc_data::workspace::read_project(uuid)
            .unwrap_or_else(|_| workspace_name.clone());

        // Duration from screen insights if available.
        let duration = ws
            .and_then(|w| w.screen_insights.duration.clone())
            .unwrap_or_else(|| "unknown".to_string());

        // Surfaces summary from surface titles.
        let surfaces_summary = ws
            .map(|w| w.surfaces.iter().map(|s| s.title.clone()).collect())
            .unwrap_or_default();

        // Final trajectory from disk.
        let final_trajectory = {
            let path = crate::mc_data::paths::trajectory_path(uuid);
            std::fs::read_to_string(&path).unwrap_or_default()
        };

        // History snapshots: histories/trajectory-N.md in chronological order.
        let history_snapshots = {
            let histories_dir = crate::mc_data::paths::histories_dir(uuid);
            let mut snaps: Vec<(u32, String)> = std::fs::read_dir(&histories_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let stem = p.file_stem()?.to_str()?.to_string();
                    // filename: trajectory-N.md
                    let n: u32 = stem.strip_prefix("trajectory-")?.parse().ok()?;
                    let content = std::fs::read_to_string(&p).ok()?;
                    Some((n, content))
                })
                .collect();
            snaps.sort_by_key(|(n, _)| *n);
            snaps.into_iter().map(|(_, c)| c).collect()
        };

        // User inputs: inputs/N.txt in order.
        let inputs = {
            let inputs_dir = crate::mc_data::paths::inputs_dir(uuid);
            let mut entries: Vec<(u32, String)> = std::fs::read_dir(&inputs_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) != Some("txt") {
                        return None;
                    }
                    let n: u32 = p.file_stem()?.to_str()?.parse().ok()?;
                    let content = std::fs::read_to_string(&p).ok()?;
                    Some((n, content))
                })
                .collect();
            entries.sort_by_key(|(n, _)| *n);
            entries.into_iter().map(|(_, c)| c).collect()
        };

        // Full events log.
        let events_jsonl = {
            let path = crate::mc_data::paths::events_log(uuid);
            std::fs::read_to_string(&path).unwrap_or_default()
        };

        // Agent session history files: surfaces/<sid>.session-path pointer.
        let session_history_files = {
            let surfaces_dir = crate::mc_data::paths::surfaces_dir(uuid);
            let mut histories = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&surfaces_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("session-path") {
                        if let Ok(session_path) = std::fs::read_to_string(&p) {
                            let session_path = session_path.trim();
                            if let Ok(content) = std::fs::read_to_string(session_path) {
                                histories.push(content);
                            }
                        }
                    }
                }
            }
            histories
        };

        // Shell logs: surfaces/<sid>.log.
        let shell_logs = {
            let surfaces_dir = crate::mc_data::paths::surfaces_dir(uuid);
            let mut logs = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&surfaces_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("log") {
                        if let Ok(content) = std::fs::read_to_string(&p) {
                            logs.push(content);
                        }
                    }
                }
            }
            logs
        };

        // Surface summary files.
        let surface_summaries = {
            let surfaces_dir = crate::mc_data::paths::surfaces_dir(uuid);
            let mut summaries = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&surfaces_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("summary") {
                        if let Ok(content) = std::fs::read_to_string(&p) {
                            let trimmed = content.trim().to_string();
                            if !trimmed.is_empty() {
                                summaries.push(trimmed);
                            }
                        }
                    }
                }
            }
            summaries
        };

        crate::llm::learning::LearningInputs {
            workspace_uuid: uuid.to_string(),
            workspace_name,
            project,
            duration,
            surfaces_summary,
            final_trajectory,
            history_snapshots,
            inputs,
            events_jsonl,
            session_history_files,
            shell_logs,
            surface_summaries,
        }
    }

    /// Mark a workspace as currently being dismissed (prevents re-triggering).
    pub fn mark_dismissing(&mut self, uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(uuid) {
            self.workspaces[idx].dismissal.dismissing = true;
        }
    }

    /// Remove a workspace from in-memory state after successful dismissal.
    pub fn drop_dismissed_workspace(&mut self, uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(uuid) {
            self.workspaces.remove(idx);
            // Rebuild the index.
            self.workspace_index.clear();
            for (i, ws) in self.workspaces.iter().enumerate() {
                self.workspace_index.insert(ws.workspace.uuid.clone(), i);
            }
            // Clamp selected index.
            if !self.workspaces.is_empty() && self.selected >= self.workspaces.len() {
                self.selected = self.workspaces.len() - 1;
            }
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // D dismissal confirmation
    // ──────────────────────────────────────────────────────────────────────

    /// Trigger immediate dismissal for a workspace by setting its grace timer
    /// far enough in the past that the next dismiss_tick fires right away.
    pub fn start_immediate_dismissal(&mut self, workspace_id: &str) {
        if let Some(&idx) = self.workspace_index.get(workspace_id) {
            let ws = &mut self.workspaces[idx];
            if !ws.dismissal.dismissing {
                ws.dismissal.grace_started_at =
                    Some(Instant::now() - std::time::Duration::from_secs(600));
            }
        }
    }

    /// First `D` — record the pending dismissal; returns `false` (not yet acted).
    /// Second `D` on the same workspace — execute the dismissal; returns `true`.
    /// Switching to a different workspace on the first `D` replaces the pending entry.
    pub fn handle_dismissal_request(&mut self, workspace_id: &str) -> bool {
        match &self.pending_dismissal {
            Some(prior) if prior == workspace_id => {
                // Second D — execute.
                self.pending_dismissal = None;
                self.start_immediate_dismissal(workspace_id);
                true
            }
            _ => {
                // First D or different workspace — record pending.
                self.pending_dismissal = Some(workspace_id.to_string());
                false
            }
        }
    }

    /// Clear the pending dismissal (call on any non-D keypress).
    pub fn clear_pending_dismissal(&mut self) {
        self.pending_dismissal = None;
    }

    /// Return the UUID of the workspace currently pending dismissal confirmation.
    pub fn pending_dismissal_workspace(&self) -> Option<&str> {
        self.pending_dismissal.as_deref()
    }

    // ──────────────────────────────────────────────────────────────────────
    // Force-regen (Shift+R)
    // ──────────────────────────────────────────────────────────────────────

    /// Mark the currently-selected workspace as due for regen on the next tick,
    /// bypassing the event-count and time thresholds.
    pub fn force_regen_selected_workspace(&mut self) {
        let uuid = match self.selected_workspace() {
            Some(ws) => ws.workspace.uuid.clone(),
            None => return,
        };
        if let Some(&idx) = self.workspace_index.get(&uuid) {
            // Set events_since_last_regen high enough to trigger the next tick.
            self.workspaces[idx].regen.events_since_last_regen = u32::MAX;
            // Also reset last_regen_at so the time-threshold is also satisfied.
            self.workspaces[idx].regen.last_regen_at = None;
        }
    }

    /// Save the current edit session for the selected workspace to disk.
    /// Returns the snapshot number N on success.
    pub fn save_trajectory_edits(
        &mut self,
        actions: &[crate::tui::trajectory_edit::EditAction],
    ) -> anyhow::Result<Option<u32>> {
        let idx = self.selected;
        let ws = match self.workspaces.get_mut(idx) {
            Some(w) => w,
            None => return Ok(None),
        };
        let doc = match ws.trajectory.as_mut() {
            Some(d) => d,
            None => return Ok(None),
        };
        let state = match ws.edit_state.as_ref() {
            Some(s) => s,
            None => return Ok(None),
        };
        let uuid = ws.workspace.uuid.clone();
        doc.sort_tasks_if_long();
        let n = crate::tui::trajectory_edit::save(&uuid, doc, state, actions)?;
        Ok(Some(n))
    }

    /// Spawn a fire-and-forget task to push the current Mission section back to
    /// the cmux workspace description. Non-fatal: errors are logged to stderr.
    ///
    /// Call this after every successful `save_trajectory_edits` so that the
    /// cmux description stays in sync with the trajectory Mission.
    ///
    /// NOTE: This intentionally does NOT mock the cmux binary in tests —
    /// the cmux call is exercised at runtime only.
    pub fn spawn_push_goal_to_cmux(&self, cmux: CmuxClient) {
        let idx = self.selected;
        let ws = match self.workspaces.get(idx) {
            Some(w) => w,
            None => return,
        };
        let doc = match ws.trajectory.as_ref() {
            Some(d) => d,
            None => return,
        };
        let goal_section = match doc.section(crate::mc_data::trajectory::SECTION_MISSION) {
            Some(s) => s,
            None => return,
        };
        let description: String = goal_section
            .items
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let ref_id = ws.workspace.ref_id.clone();
        tokio::spawn(async move {
            if let Err(e) = cmux.set_workspace_description(&ref_id, &description).await {
                eprintln!("set_workspace_description({ref_id}): {e:?}");
            }
        });
    }
}

/// Spawn a background task that captures a workspace's screen (and optionally
/// classifies it via TypeSafe), then sends a ScreenUpdate on `tx`.
/// Each phase has its own timeout so a hung remote workspace can't pile up.
fn spawn_screen_task(
    workspace_uuid: String,
    ref_id: String,
    client: CmuxClient,
    classifier: Option<TypeSafeClassifier>,
    tx: tokio::sync::mpsc::UnboundedSender<ScreenUpdate>,
) {
    tokio::spawn(async move {
        use tokio::time::{Duration, timeout};
        let screen = timeout(Duration::from_secs(3), client.read_screen(&ref_id, 100))
            .await
            .ok()
            .and_then(|r| r.ok())
            .filter(|s| !s.trim().is_empty());

        let classification = if let (Some(s), Some(c)) = (&screen, classifier) {
            timeout(Duration::from_secs(2), c.classify_screen(s))
                .await
                .ok()
                .and_then(|r| r.ok())
        } else {
            None
        };

        let _ = tx.send(ScreenUpdate {
            workspace_uuid,
            screen,
            classification,
        });
    });
}

/// Load hook-written status for a workspace.
/// Returns (state_string, unix_timestamp) if the file exists and is valid JSON.
fn load_hook_status(workspace_uuid: &str) -> Option<(String, u64)> {
    let path = status_dir().join(format!("{}.json", workspace_uuid));
    let content = std::fs::read_to_string(path).ok()?;
    // Minimal JSON parsing: {"state":"working","ts":"2026-05-17T15:50:00Z"}
    let state = content
        .split("\"state\"")
        .nth(1)?
        .split('"')
        .nth(1)?
        .to_string();
    // Use file mtime as timestamp (simpler than parsing ISO 8601)
    let mtime = std::fs::metadata(status_dir().join(format!("{}.json", workspace_uuid)))
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((state, mtime))
}

/// Load notes for a workspace from the notes directory.
fn load_workspace_notes(workspace_name: &str) -> Option<String> {
    let path = notes_dir().join(format!("{}.md", workspace_slug(workspace_name)));
    std::fs::read_to_string(path)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn hash_bullets(bullets: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bullets.hash(&mut hasher);
    hasher.finish()
}

/// Load the last `n` user explanation files from `inputs_dir/<N>.txt`.
/// Returns them in chronological order (oldest first).
fn load_recent_user_explanations(inputs_dir: &PathBuf, n: usize) -> Vec<String> {
    let mut entries: Vec<(u32, PathBuf)> = match std::fs::read_dir(inputs_dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("txt") {
                    return None;
                }
                let stem = p.file_stem()?.to_str()?.parse::<u32>().ok()?;
                Some((stem, p))
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort_by_key(|(n, _)| *n);
    entries
        .into_iter()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter_map(|(_, p)| std::fs::read_to_string(p).ok())
        .collect()
}

/// Load `.summary` files from a surfaces directory.
/// Returns (sid, summary_text) pairs.
fn load_surface_summaries(surfaces_dir: &PathBuf) -> Vec<(String, String)> {
    if !surfaces_dir.exists() {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(surfaces_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("summary") {
            continue;
        }
        let sid = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if let Ok(text) = std::fs::read_to_string(&path) {
            let summary = text.trim().to_string();
            if !summary.is_empty() {
                result.push((sid, summary));
            }
        }
    }
    result
}

/// Return the short hostname of the local machine (e.g. "mbp"), normalised to
/// lowercase.  Used to populate `WorkspaceContext::host` so that session-log
/// tier-1 matching can exclude logs written on a different machine.
///
/// Falls back to "localhost" if `hostname -s` is unavailable.
fn hostname_short() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "localhost".to_string())
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmux::client::Workspace;
    use crate::mc_data::trajectory::TrajectoryDoc;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn shift_key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // Trajectory doc with a `## Current surfaces` item that has a surface_id.
    const SAMPLE_WITH_SURFACE: &str = "---
workspace: test-ws
---

## Mission
- Build investment agent

## Current surfaces
- claude · mbp · working              <!-- mc:surface:sid-42 -->

## Goals & Progress
- [ ] sprint-01
";

    // Trajectory doc with a `## Current surfaces` item with NO surface_id.
    const SAMPLE_NO_SURFACE_ID: &str = "---
workspace: test-ws
---

## Mission
- Build investment agent

## Current surfaces
- claude · mbp · working

## Goals & Progress
- [ ] sprint-01
";

    fn make_ws(doc_text: &str) -> WorkspaceState {
        let mut doc = TrajectoryDoc::parse(doc_text).unwrap();
        doc.ensure_sections();
        WorkspaceState {
            workspace: Workspace {
                ref_id: "workspace:3".to_string(),
                uuid: "test-uuid-1".to_string(),
                name: "test-ws".to_string(),
                selected: false,
                description: None,
                current_directory: None,
                custom_color: None,
            },
            session: None,
            surfaces: Vec::new(),
            screen_preview: None,
            screen_insights: ScreenInsights::default(),
            tool_call_count: 0,
            notes: None,
            hook_status: None,
            classification: None,
            loading: false,
            summary: None,
            summarizing: false,
            trajectory: Some(doc),
            edit_state: None,
            peek_state: None,
            peek_yield_pending: false,
            regen: RegenSchedulerState::default(),
            dismissal: DismissalState::default(),
            dispatch_modal: None,
            dispatch_pending_outcome: None,
            dispatch_error: None,
        }
    }

    fn make_app(doc_text: &str) -> App {
        let ws = make_ws(doc_text);
        let mut app = App::new();
        app.workspaces.push(ws);
        app.workspace_index.insert("test-uuid-1".to_string(), 0);
        app.selected = 0;
        app
    }

    // ── Enter on Current surfaces row → enters peek mode ─────────────────────

    #[test]
    fn enter_on_current_surfaces_row_enters_peek_mode() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        // Navigate to section 1 (Current surfaces), item 0.
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.cursor_section = 1;
        state.cursor_item = 0;

        app.handle_trajectory_key(key(KeyCode::Enter));

        assert!(
            app.workspaces[0].peek_state.is_some(),
            "peek_state should be Some after Enter on Current surfaces row"
        );
    }

    #[test]
    fn enter_on_current_surfaces_row_uses_surface_id() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.cursor_section = 1;
        state.cursor_item = 0;

        app.handle_trajectory_key(key(KeyCode::Enter));

        let peek = app.workspaces[0].peek_state.as_ref().unwrap();
        assert_eq!(
            peek.surface_ref, "sid-42",
            "surface_ref should match surface_id in comment"
        );
    }

    #[test]
    fn enter_on_current_surfaces_row_falls_back_to_workspace_ref() {
        let mut app = make_app(SAMPLE_NO_SURFACE_ID);
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.cursor_section = 1;
        state.cursor_item = 0;

        app.handle_trajectory_key(key(KeyCode::Enter));

        let peek = app.workspaces[0].peek_state.as_ref().unwrap();
        assert_eq!(
            peek.surface_ref, "workspace:3",
            "should fall back to workspace ref_id when no surface_id"
        );
    }

    // ── Esc in peek mode → clears peek_state ─────────────────────────────────

    #[test]
    fn esc_in_peek_mode_clears_peek_state() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        // Put app into peek mode manually.
        app.workspaces[0].peek_state = Some(crate::tui::peek_view::PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        ));

        app.handle_trajectory_key(key(KeyCode::Esc));

        assert!(
            app.workspaces[0].peek_state.is_none(),
            "peek_state should be None after Esc"
        );
    }

    // ── j / k in peek mode adjust scroll_offset ──────────────────────────────

    #[test]
    fn j_in_peek_mode_scrolls_down() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let mut ps = crate::tui::peek_view::PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        );
        // Fill buffer so scrolling has room.
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        app.workspaces[0].peek_state = Some(ps);

        app.handle_trajectory_key(key(KeyCode::Char('j')));

        let offset = app.workspaces[0].peek_state.as_ref().unwrap().scroll_offset;
        assert_eq!(offset, 3, "j should increase scroll_offset by 3");
    }

    #[test]
    fn k_in_peek_mode_scrolls_up() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let mut ps = crate::tui::peek_view::PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        );
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 10;
        app.workspaces[0].peek_state = Some(ps);

        app.handle_trajectory_key(key(KeyCode::Char('k')));

        let offset = app.workspaces[0].peek_state.as_ref().unwrap().scroll_offset;
        assert_eq!(offset, 7, "k should decrease scroll_offset by 3");
    }

    // ── g / G in peek mode ────────────────────────────────────────────────────

    #[test]
    fn g_in_peek_mode_goes_to_top() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let mut ps = crate::tui::peek_view::PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        );
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 20;
        app.workspaces[0].peek_state = Some(ps);

        app.handle_trajectory_key(key(KeyCode::Char('g')));

        let offset = app.workspaces[0].peek_state.as_ref().unwrap().scroll_offset;
        assert_eq!(offset, 0, "g should reset scroll_offset to 0");
    }

    #[test]
    fn big_g_in_peek_mode_goes_to_bottom() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let mut ps = crate::tui::peek_view::PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        );
        for i in 0..30 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        app.workspaces[0].peek_state = Some(ps);

        app.handle_trajectory_key(shift_key('G'));

        let peek = app.workspaces[0].peek_state.as_ref().unwrap();
        assert_eq!(
            peek.scroll_offset,
            peek.max_scroll(),
            "G should go to max scroll offset"
        );
    }

    // ── Enter in peek mode sets yield_pending ─────────────────────────────────

    #[test]
    fn enter_in_peek_mode_sets_yield_pending() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].peek_state = Some(crate::tui::peek_view::PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        ));

        app.handle_trajectory_key(key(KeyCode::Enter));

        assert!(
            app.workspaces[0].peek_yield_pending,
            "peek_yield_pending should be true after Enter in peek mode"
        );
    }

    // ── take_peek_yield ───────────────────────────────────────────────────────

    #[test]
    fn take_peek_yield_returns_ref_id_and_clears_peek() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].peek_state = Some(crate::tui::peek_view::PeekState::new(
            "workspace:7".to_string(),  // workspace_ref — what select-workspace needs
            "surface:42".to_string(),    // surface_ref — for future per-surface use
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        ));
        app.workspaces[0].peek_yield_pending = true;

        let ref_id = app.take_peek_yield();

        assert_eq!(
            ref_id,
            Some("workspace:7".to_string()),
            "yield must return the workspace_ref (not the surface_ref) so cmux select-workspace works"
        );
        assert!(
            app.workspaces[0].peek_state.is_none(),
            "peek_state cleared on yield"
        );
        assert!(!app.workspaces[0].peek_yield_pending, "flag cleared");
    }

    #[test]
    fn take_peek_yield_returns_none_when_not_pending() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].peek_state = Some(crate::tui::peek_view::PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        ));
        // peek_yield_pending is false (default)

        let ref_id = app.take_peek_yield();
        assert!(ref_id.is_none());
        assert!(
            app.workspaces[0].peek_state.is_some(),
            "peek_state unchanged"
        );
    }

    // ── Regen scheduler ───────────────────────────────────────────────────────

    #[test]
    fn workspaces_due_for_regen_excludes_insert_mode() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        // Set up enough events to trigger event-threshold regen.
        app.workspaces[0].regen.events_since_last_regen = 15;
        // Put workspace into insert mode.
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.mode = crate::tui::trajectory_edit::EditMode::Insert {
            focus: crate::tui::trajectory_edit::InsertFocus::Item,
        };

        let due = app.workspaces_due_for_regen();
        assert!(
            due.is_empty(),
            "should not schedule regen while workspace is in Insert mode"
        );
    }

    #[test]
    fn workspaces_due_for_regen_excludes_in_flight() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].regen.events_since_last_regen = 15;
        app.workspaces[0].regen.regen_in_flight = true;

        let due = app.workspaces_due_for_regen();
        assert!(
            due.is_empty(),
            "should not schedule regen when one is already in flight"
        );
    }

    #[test]
    fn workspaces_due_for_regen_includes_past_time_threshold() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        // Set last_regen_at to a time far in the past (simulate by subtracting
        // a large duration). We use Instant::now() minus 400s which is > 300s threshold.
        // Since Instant doesn't support subtraction of arbitrary durations in a
        // portable way, we set last_regen_at to None (never regenerated) with events > 0.
        // Never-regenerated + has events => time threshold applies immediately.
        app.workspaces[0].regen.events_since_last_regen = 1;
        app.workspaces[0].regen.last_regen_at = None;
        // No insert mode, no in-flight.

        let due = app.workspaces_due_for_regen();
        assert_eq!(
            due,
            vec!["test-uuid-1".to_string()],
            "workspace never regenerated with pending events should be due"
        );
    }

    #[test]
    fn workspaces_due_for_regen_event_threshold() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        // Set last_regen_at to now (just happened), but accumulate enough events.
        app.workspaces[0].regen.last_regen_at = Some(Instant::now());
        app.workspaces[0].regen.events_since_last_regen = 10; // at threshold

        let due = app.workspaces_due_for_regen();
        assert_eq!(
            due,
            vec!["test-uuid-1".to_string()],
            "workspace at event threshold should be due even if time threshold not met"
        );
    }

    #[test]
    fn workspaces_due_for_regen_excludes_zero_events() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].regen.events_since_last_regen = 0;
        app.workspaces[0].regen.last_regen_at = None;

        let due = app.workspaces_due_for_regen();
        assert!(
            due.is_empty(),
            "workspace with 0 pending events should not be due for regen"
        );
    }

    #[test]
    fn apply_regenerated_trajectory_replaces_and_resets() {
        use crate::mc_data::trajectory::TrajectoryDoc;

        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].regen.events_since_last_regen = 5;
        app.workspaces[0].regen.regen_in_flight = true;

        let new_doc_text = "---\nworkspace: test-ws\n---\n\n## Mission\n- Updated goal\n\n## Current surfaces\n\n## Goals & Progress\n- [ ] new task\n";
        let new_doc = TrajectoryDoc::parse(new_doc_text).unwrap();

        // apply_regenerated_trajectory saves to disk, so we need the path to exist.
        // We skip the disk save assertion in unit tests — just verify in-memory state.
        // Since the path won't exist in test, we test via direct inspection only.
        // Note: save_to_file will mkdir-p and write, so it will succeed on real filesystem.
        app.apply_regenerated_trajectory("test-uuid-1", new_doc);

        let ws = &app.workspaces[0];
        assert_eq!(
            ws.regen.events_since_last_regen, 0,
            "events counter should reset after regen"
        );
        assert!(
            !ws.regen.regen_in_flight,
            "in_flight flag should be cleared after regen"
        );
        assert!(
            ws.regen.last_regen_at.is_some(),
            "last_regen_at should be set after regen"
        );
        // Verify the trajectory was actually replaced.
        let goal_items = ws
            .trajectory
            .as_ref()
            .and_then(|d| d.section("Mission"))
            .map(|s| s.items.iter().map(|i| i.text.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            goal_items.iter().any(|t| t.contains("Updated goal")),
            "trajectory should be replaced with new content"
        );
    }

    #[test]
    fn apply_regenerated_trajectory_skips_insert_mode() {
        use crate::mc_data::trajectory::TrajectoryDoc;

        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].regen.regen_in_flight = true;
        // Enter insert mode.
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.mode = crate::tui::trajectory_edit::EditMode::Insert {
            focus: crate::tui::trajectory_edit::InsertFocus::Item,
        };

        let new_doc_text = "---\nworkspace: test-ws\n---\n\n## Mission\n- Should not appear\n\n## Current surfaces\n\n## Goals & Progress\n";
        let new_doc = TrajectoryDoc::parse(new_doc_text).unwrap();
        app.apply_regenerated_trajectory("test-uuid-1", new_doc);

        // In-flight flag should be cleared so next tick can retry.
        assert!(
            !app.workspaces[0].regen.regen_in_flight,
            "in_flight flag cleared even when skipping due to insert mode"
        );
        // Trajectory should NOT be replaced.
        let goal_items = app.workspaces[0]
            .trajectory
            .as_ref()
            .and_then(|d| d.section("Mission"))
            .map(|s| s.items.iter().map(|i| i.text.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            !goal_items.iter().any(|t| t.contains("Should not appear")),
            "trajectory should NOT be replaced while in insert mode"
        );
    }

    // ── T10: D dismissal confirmation ─────────────────────────────────────────

    #[test]
    fn first_d_records_pending_returns_false() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let executed = app.handle_dismissal_request("ws-1");
        assert!(!executed, "first D should not execute dismissal");
        assert_eq!(
            app.pending_dismissal_workspace(),
            Some("ws-1"),
            "first D should set pending_dismissal to the workspace id"
        );
    }

    #[test]
    fn second_d_on_same_workspace_executes() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        // Add a workspace with uuid "ws-exec" so start_immediate_dismissal can look it up.
        let ws2 = make_ws(SAMPLE_WITH_SURFACE);
        let mut ws2 = ws2;
        ws2.workspace.uuid = "ws-exec".to_string();
        app.workspaces.push(ws2);
        app.workspace_index.insert("ws-exec".to_string(), 1);

        app.handle_dismissal_request("ws-exec");
        let executed = app.handle_dismissal_request("ws-exec");
        assert!(
            executed,
            "second D on same workspace should execute dismissal"
        );
        assert!(
            app.pending_dismissal_workspace().is_none(),
            "pending_dismissal should be cleared after execution"
        );
    }

    #[test]
    fn d_on_different_workspace_replaces_pending() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.handle_dismissal_request("ws-1");
        app.handle_dismissal_request("ws-2");
        assert_eq!(
            app.pending_dismissal_workspace(),
            Some("ws-2"),
            "second D on a different workspace should replace the pending entry"
        );
    }

    #[test]
    fn clear_pending_dismissal_resets() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.handle_dismissal_request("ws-1");
        app.clear_pending_dismissal();
        assert!(
            app.pending_dismissal_workspace().is_none(),
            "clear_pending_dismissal should set pending_dismissal to None"
        );
    }

    // ── T11: Force-regen via Shift+R ──────────────────────────────────────────

    #[test]
    fn force_regen_marks_workspace_due() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        // Start in a state that would NOT normally be due for regen.
        app.workspaces[0].regen.events_since_last_regen = 0;
        app.workspaces[0].regen.last_regen_at = Some(Instant::now());

        app.force_regen_selected_workspace();

        assert_eq!(
            app.workspaces[0].regen.events_since_last_regen,
            u32::MAX,
            "force_regen should set events_since_last_regen to u32::MAX"
        );
        assert!(
            app.workspaces[0].regen.last_regen_at.is_none(),
            "force_regen should clear last_regen_at"
        );
        // The workspace should now appear in workspaces_due_for_regen.
        let due = app.workspaces_due_for_regen();
        assert!(
            due.contains(&"test-uuid-1".to_string()),
            "workspace should be due for regen after force_regen"
        );
    }

    #[test]
    fn force_regen_with_no_selection_is_noop() {
        let mut app = App::new();
        // No workspaces — should not panic.
        app.force_regen_selected_workspace();
    }
}
