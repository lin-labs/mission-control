use crate::cmux::client::{CmuxClient, SurfaceInfo, Workspace};
use crate::cmux::events::AgentEvent;
use crate::llm::Summary;
use crate::llm::trajectory_regen::RegenInputs;
use crate::llm::typesafe::{ScreenClassification, TypeSafeClassifier};
use crate::mc_data::mux_state::MuxSessionState;
use crate::session::file::{self, SessionFile};
use crate::session::watcher::FileChanged;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Directory for persistent per-workspace notes.
pub fn notes_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/mission-control/notes")
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
    /// Exact arcmux mesh state keyed by the local cmux `surface:N` ref. The
    /// join is authorized only by stable surface/workspace UUID bindings.
    pub remote_surfaces: HashMap<String, crate::mc_data::arcmux_mesh::RemoteSurfaceState>,
    pub screen_preview: Option<String>,
    pub screen_insights: ScreenInsights,
    pub tool_call_count: u32,
    /// Persistent user notes (loaded from ~/.config/mission-control/notes/).
    pub notes: Option<String>,
    /// Activity status read from the centralized mux protocol state.
    pub mux_status: Option<MuxSessionState>,
    /// TypeSafe AI classification of screen content.
    pub classification: Option<ScreenClassification>,
    /// True while a background screen refresh / classification is in flight.
    pub loading: bool,
    /// LLM-generated summary (independent of session.trajectory so it can be
    /// produced even when no session file matches this workspace).
    pub summary: Option<Summary>,
    /// Repo-local Beads issues for repos detected across this workspace's
    /// surfaces, when any detected repo has a `.beads/` store.
    pub beads: Option<crate::mc_data::beads::WorkspaceBeadsView>,
    /// Registry-selected Linear projection. When present it is authoritative
    /// over any repo-local `.beads/` directory.
    pub linear: Option<crate::mc_data::linear::WorkspaceLinearView>,
    /// Exact validated `linear://` target requested by Enter on a projected
    /// issue row. The main event loop consumes it and launches the macOS app.
    pub linear_open_pending: Option<String>,
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
    pub dispatch_pending_outcome: Option<crate::tui::dispatch_modal::DispatchOutcome>,
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

/// One captured frame from a remote (mosh/ssh) surface, flowing back from a
/// background grab task to [`App::apply_remote_grab`].
pub struct RemoteGrabUpdate {
    pub workspace_uuid: String,
    pub surface_ref: String,
    pub raw: String,
}

/// An LLM-inferred intent for a remote surface, flowing back from a background
/// provider task to [`App::apply_remote_intent`].
pub struct RemoteIntentUpdate {
    pub surface_ref: String,
    pub intent: crate::mc_data::surface_render::SurfaceIntentSummary,
}

/// Regenerate a surface's "overall" summary only after this many new user turns
/// (significant change), not every turn.
const OVERALL_SUMMARY_EVERY: usize = 4;

impl WorkspaceState {
    /// Whether this workspace is a window onto a remote host: any surface has a
    /// mosh/ssh client in the foreground. Remote panes often lack local mux
    /// protocol state, so this gates the TypeSafe classifier, which we only
    /// spend on remote workspaces.
    pub fn is_remote(&self) -> bool {
        self.has_active_remote_surfaces()
            || self
                .surfaces
                .iter()
                .any(|s| s.kind == crate::mc_data::surface_kind::SurfaceKind::Remote)
    }

    /// Legacy terminal inference is used only for unbound mosh/ssh surfaces.
    /// Bound remotes get their state from the mesh projection instead.
    fn needs_remote_screen_inference(&self) -> bool {
        self.is_remote()
            && self.surfaces.iter().any(|s| {
                s.kind == crate::mc_data::surface_kind::SurfaceKind::Remote
                    && !self.remote_surfaces.contains_key(&s.ref_id)
            })
    }

    /// The arcmux turn contract for this workspace's bound session, when it
    /// carries goal artifacts worth showing. Authoritative agent-written state.
    pub fn turn_contract(&self) -> Option<&crate::mc_data::mux_state::TurnContract> {
        if self.has_active_remote_surfaces() && !self.has_local_agent_surface() {
            return None;
        }
        self.mux_status.as_ref().and_then(|s| s.contract())
    }

    /// Derive the agent name from mux state, session, screen insights, or surface titles.
    pub fn agent_name(&self) -> &str {
        if !self.has_active_remote_surfaces() || self.has_local_agent_surface() {
            if let Some(ref status) = self.mux_status {
                if !status.agent.is_empty() {
                    return &status.agent;
                }
            }
            if let Some(ref session) = self.session {
                if let Some(ref agent) = session.frontmatter.agent {
                    return agent;
                }
            }
            if let Some(ref agent) = self.screen_insights.agent {
                return agent;
            }
            for s in &self.surfaces {
                if self.remote_surfaces.contains_key(&s.ref_id) {
                    continue;
                }
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
        }
        self.active_remote_surfaces()
            .into_iter()
            .find_map(|(_, remote)| remote.agent.as_deref())
            .unwrap_or("")
    }

    /// Get working directory from screen insights or surface titles.
    pub fn working_dir(&self) -> &str {
        if !self.has_active_remote_surfaces() || self.has_local_agent_surface() {
            if let Some(ref dir) = self.screen_insights.working_dir {
                return dir;
            }
        } else if let Some(dir) = self
            .active_remote_surfaces()
            .into_iter()
            .find_map(|(_, remote)| remote.launch_cwd.as_deref())
        {
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
    /// Priority: mux protocol state > TypeSafe classification > screen activity > surface detection.
    pub fn agent_state(&self) -> AgentState {
        let local = self.local_agent_state();
        let mut actionable = local == AgentState::NeedsMe;
        let mut working = local == AgentState::Working;
        let mut stale = false;
        for remote in self.remote_surfaces.values() {
            match remote.freshness {
                crate::mc_data::arcmux_mesh::RemoteFreshness::Fresh => match remote.activity() {
                    crate::mc_data::arcmux_mesh::RemoteActivity::Actionable => actionable = true,
                    crate::mc_data::arcmux_mesh::RemoteActivity::Working => working = true,
                    crate::mc_data::arcmux_mesh::RemoteActivity::Idle => {}
                },
                crate::mc_data::arcmux_mesh::RemoteFreshness::Syncing
                | crate::mc_data::arcmux_mesh::RemoteFreshness::Stale => stale = true,
                crate::mc_data::arcmux_mesh::RemoteFreshness::Gone => {}
            }
        }
        if actionable {
            AgentState::NeedsMe
        } else if working {
            AgentState::Working
        } else if stale {
            AgentState::Stale
        } else {
            AgentState::Idle
        }
    }

    fn has_local_agent_surface(&self) -> bool {
        self.surfaces.iter().any(|s| {
            if self.remote_surfaces.contains_key(&s.ref_id) {
                return false;
            }
            let t = s.title.to_ascii_lowercase();
            s.kind.is_agent()
                || t.contains("claude")
                || t.contains("codex")
                || t.contains("opencode")
        })
    }

    fn has_active_remote_surfaces(&self) -> bool {
        self.remote_surfaces
            .values()
            .any(|remote| remote.freshness != crate::mc_data::arcmux_mesh::RemoteFreshness::Gone)
    }

    /// Stable active-remote ordering prevents header identity and cwd from
    /// flickering when a workspace contains multiple bound remote surfaces.
    /// Actionable work wins, then working, then idle/offline; surface ref is
    /// the deterministic tie-breaker. Gone rows are folded history.
    fn active_remote_surfaces(
        &self,
    ) -> Vec<(&str, &crate::mc_data::arcmux_mesh::RemoteSurfaceState)> {
        let mut remotes: Vec<_> = self
            .remote_surfaces
            .iter()
            .filter(|(_, remote)| {
                remote.freshness != crate::mc_data::arcmux_mesh::RemoteFreshness::Gone
            })
            .map(|(surface_ref, remote)| (surface_ref.as_str(), remote))
            .collect();
        remotes.sort_by_key(|(surface_ref, remote)| {
            let priority = match remote.activity() {
                crate::mc_data::arcmux_mesh::RemoteActivity::Actionable => 0,
                crate::mc_data::arcmux_mesh::RemoteActivity::Working => 1,
                crate::mc_data::arcmux_mesh::RemoteActivity::Idle => 2,
            };
            (priority, *surface_ref)
        });
        remotes
    }

    fn local_agent_state(&self) -> AgentState {
        // Workspace-wide screen/session facts are unsafe for an all-remote
        // workspace: they can describe whichever local surface was newest.
        // Keep them only when there is no exact remote binding, or when a
        // distinct local agent surface also exists.
        let local_fallbacks_allowed =
            !self.has_active_remote_surfaces() || self.has_local_agent_surface();
        // Central mux state is the authoritative activity source. Native hooks
        // write exactly one protocol store via `arcmux hook`; mc only reads
        // these JSON docs and does not infer working/idle from hook event names.
        if local_fallbacks_allowed && let Some(ref status) = self.mux_status {
            if status.working {
                return AgentState::Working;
            }
            if status.has_ended_turn() || status.last_event == "notification" {
                return AgentState::NeedsMe;
            }
            return AgentState::Idle;
        }

        // TypeSafe AI classification (sub-100ms, high confidence)
        if local_fallbacks_allowed && let Some(ref cls) = self.classification {
            if cls.state_confidence > 0.6 {
                return match cls.state.as_str() {
                    "working" => AgentState::Working,
                    "waiting" => AgentState::NeedsMe,
                    _ => AgentState::Idle,
                };
            }
        }

        // Derive from screen activity
        if local_fallbacks_allowed && let Some(ref activity) = self.screen_insights.activity {
            // Ongoing: contains ellipsis or parenthesized timing
            // e.g., "✻ Puzzling… (2m 30s)" or "• Working (54s · esc to interrupt)"
            if activity.contains('…') || activity.contains('(') {
                return AgentState::Working;
            }
            // Completed: "✻ Cooked for 11m 29s" (no parens, no ellipsis)
            return AgentState::NeedsMe;
        }

        // Agent detected but no activity → waiting for input
        if self.has_local_agent_surface() {
            AgentState::NeedsMe
        } else {
            AgentState::Idle
        }
    }

    /// Derive the host from session or surface titles.
    pub fn host_name(&self) -> String {
        let mut devices: Vec<&str> = self
            .remote_surfaces
            .values()
            .filter(|remote| remote.freshness != crate::mc_data::arcmux_mesh::RemoteFreshness::Gone)
            .map(|remote| remote.locator.device_id.as_str())
            .collect();
        devices.sort_unstable();
        devices.dedup();
        if devices.len() == 1 {
            return devices[0].to_string();
        }
        if devices.len() > 1 {
            return format!("remote×{}", devices.len());
        }
        if let Some(ref session) = self.session {
            if let Some(ref host) = session.frontmatter.host {
                return host.clone();
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
                        Some(host.to_string())
                    }
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }
}

fn remote_surface_is_current(
    remote: Option<&crate::mc_data::arcmux_mesh::RemoteSurfaceState>,
) -> bool {
    !remote.is_some_and(|state| {
        state.freshness == crate::mc_data::arcmux_mesh::RemoteFreshness::Gone
    })
}

fn retain_remote_surfaces_as_stale(
    old: &HashMap<String, crate::mc_data::arcmux_mesh::RemoteSurfaceState>,
    surfaces: &[SurfaceInfo],
) -> HashMap<String, crate::mc_data::arcmux_mesh::RemoteSurfaceState> {
    let mut stale = old.clone();
    for remote in stale.values_mut() {
        remote.mark_stale();
    }
    stale.retain(|surface_ref, _| {
        surfaces
            .iter()
            .any(|surface| &surface.ref_id == surface_ref)
    });
    stale
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
    /// A bound remote session is retained but its mesh projection is offline.
    Stale,
}

impl AgentState {
    pub fn label(&self) -> &str {
        match self {
            AgentState::Working => "working",
            AgentState::NeedsMe => "waiting",
            AgentState::Idle => "--",
            AgentState::Stale => "offline",
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
    /// Machine-wide configuration or credential warning. Rendered in the
    /// global info layer rather than attached to any monitored workspace.
    pub global_warning: Option<String>,
    /// Sanitized health warning from the local arcmux loopback consumer.
    pub mesh_warning: Option<String>,
    session_to_workspace: HashMap<String, String>,
    workspace_index: HashMap<String, usize>,
    bullet_hashes: HashMap<PathBuf, u64>,
    beads_generation: u64,
    /// Workspace UUID awaiting the second `D` confirmation for dismissal.
    /// Set on first `D`; cleared on second `D` (executes dismissal) or any other key.
    pub pending_dismissal: Option<String>,
    /// vim-like input mode for the `:command` bar.
    pub input_mode: crate::tui::command::InputMode,
    /// Per-surface screen-grab/merge state for remote (mosh/ssh) surfaces.
    remote_watch: crate::mc_data::remote_intent::RemoteWatch,
    /// Monotonic counter incremented each remote-grab tick (drives backoff).
    remote_grab_tick: u64,
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
    pub beads_by_ws_id: HashMap<String, crate::mc_data::beads::WorkspaceBeadsView>,
    pub linear_by_ws_id: HashMap<String, crate::mc_data::linear::WorkspaceLinearView>,
    pub surface_intents_by_ws_id:
        HashMap<String, HashMap<String, crate::mc_data::session_log::ConversationIntent>>,
    pub remote_mesh: Option<crate::mc_data::arcmux_mesh::RemoteMeshSnapshot>,
    pub mesh_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BeadsRefreshTarget {
    pub generation: u64,
    pub workspace_id: String,
    pub repo_roots: Vec<PathBuf>,
    pub repo_by_surface_ref: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct BeadsRefreshSnapshot {
    pub generation: u64,
    pub beads_by_ws_id: HashMap<String, crate::mc_data::beads::WorkspaceBeadsView>,
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
    gather_refresh_snapshot_inner(client, histories_dir, false).await
}

pub async fn gather_beads_refresh_snapshot(
    targets: Vec<BeadsRefreshTarget>,
) -> BeadsRefreshSnapshot {
    tokio::task::spawn_blocking(move || {
        let generation = targets
            .first()
            .map(|target| target.generation)
            .unwrap_or_default();
        let mut repo_cache: HashMap<PathBuf, crate::mc_data::beads::BeadsView> = HashMap::new();
        let mut beads_by_ws_id = HashMap::new();

        for target in targets {
            let mut seen = std::collections::HashSet::new();
            let mut repos = Vec::new();
            for repo in target.repo_roots {
                if !seen.insert(repo.clone()) || !repo.join(".beads").is_dir() {
                    continue;
                }
                let view = repo_cache
                    .entry(repo.clone())
                    .or_insert_with(|| crate::mc_data::beads::load_for_repo_path(&repo))
                    .clone();
                repos.push(view);
            }
            if !repos.is_empty() {
                beads_by_ws_id.insert(
                    target.workspace_id,
                    crate::mc_data::beads::WorkspaceBeadsView {
                        repos,
                        repo_by_surface_ref: target.repo_by_surface_ref,
                    },
                );
            }
        }

        BeadsRefreshSnapshot {
            generation,
            beads_by_ws_id,
        }
    })
    .await
    .unwrap_or_default()
}

fn resolve_task_sources(
    workspaces: &[Workspace],
    repo_roots_by_ws_id: &HashMap<String, Vec<PathBuf>>,
    registry: Option<&crate::mc_data::project_registry::ProjectRegistry>,
) -> HashMap<String, crate::mc_data::project_registry::TaskSource> {
    use crate::mc_data::project_registry::TaskSource;

    let Some(registry) = registry else {
        return workspaces
            .iter()
            .map(|workspace| (workspace.uuid.clone(), TaskSource::Unregistered))
            .collect();
    };

    workspaces
        .iter()
        .map(|workspace| {
            // A workspace's declared identity owns its tracker even when the
            // focused surface temporarily moves current_directory into a
            // utility repo. Paths remain authoritative fallback evidence when
            // title/description do not identify one registered unit.
            let mut source = registry
                .resolve_workspace_identity(&workspace.name, workspace.description.as_deref());
            if source == TaskSource::Unregistered {
                source = workspace
                    .current_directory
                    .as_deref()
                    .map(std::path::Path::new)
                    .map(|path| registry.resolve(path))
                    .unwrap_or(TaskSource::Unregistered);
            }
            if source == TaskSource::Unregistered {
                source = repo_roots_by_ws_id
                    .get(&workspace.uuid)
                    .into_iter()
                    .flatten()
                    .map(|path| registry.resolve(path))
                    .find(|candidate| *candidate != TaskSource::Unregistered)
                    .unwrap_or(TaskSource::Unregistered);
            }
            (workspace.uuid.clone(), source)
        })
        .collect()
}

async fn gather_linear_views(
    task_sources: &HashMap<String, crate::mc_data::project_registry::TaskSource>,
) -> HashMap<String, crate::mc_data::linear::WorkspaceLinearView> {
    use crate::mc_data::linear::{LinearClient, WorkspaceLinearView};
    use crate::mc_data::project_registry::TaskSource;

    let mut views = HashMap::new();
    let grouped = linear_query_groups(task_sources);
    for (workspace_id, source) in task_sources {
        match source {
            TaskSource::Linear(_) => {}
            TaskSource::LinearUnavailable => {
                views.insert(
                    workspace_id.clone(),
                    WorkspaceLinearView {
                        project_id: String::new(),
                        required_labels: Vec::new(),
                        feature_name: None,
                        issues: Vec::new(),
                        warning: Some(
                            "Linear unavailable: project registry coordinates are incomplete"
                                .to_string(),
                        ),
                    },
                );
            }
            TaskSource::Unregistered | TaskSource::Beads => {}
        }
    }

    if grouped.is_empty() {
        return views;
    }

    let key = crate::mc_data::linear::resolve_api_key().await;
    let Some(api_key) = key.api_key else {
        let warning = key
            .warning
            .unwrap_or_else(|| "Linear unavailable: credential lookup failed".to_string());
        for ((project_id, required_labels), workspace_targets) in grouped {
            for (workspace_id, feature_name) in workspace_targets {
                views.insert(
                    workspace_id,
                    WorkspaceLinearView {
                        project_id: project_id.clone(),
                        required_labels: required_labels.clone(),
                        feature_name,
                        issues: Vec::new(),
                        warning: Some(warning.clone()),
                    },
                );
            }
        }
        return views;
    };

    let client = LinearClient::new(api_key);
    let mut queries = tokio::task::JoinSet::new();
    for ((project_id, required_labels), workspace_targets) in grouped {
        let client = client.clone();
        queries.spawn(async move {
            let result = client.fetch_issues(&project_id, &required_labels).await;
            (project_id, required_labels, workspace_targets, result)
        });
    }
    while let Some(result) = queries.join_next().await {
        let Ok((project_id, required_labels, workspace_targets, result)) = result else {
            // A task panic/cancellation is sanitized and scoped to the affected
            // query; it never brings down the TUI or exposes response data.
            continue;
        };
        let (issues, warning) = match result {
            Ok(issues) => (issues, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        for (workspace_id, feature_name) in workspace_targets {
            views.insert(
                workspace_id,
                WorkspaceLinearView {
                    project_id: project_id.clone(),
                    required_labels: required_labels.clone(),
                    feature_name,
                    issues: issues.clone(),
                    warning: warning.clone(),
                },
            );
        }
    }
    views
}

fn linear_query_groups(
    task_sources: &HashMap<String, crate::mc_data::project_registry::TaskSource>,
) -> HashMap<(String, Vec<String>), Vec<(String, Option<String>)>> {
    use crate::mc_data::project_registry::TaskSource;

    let mut grouped: HashMap<(String, Vec<String>), Vec<(String, Option<String>)>> = HashMap::new();
    for (workspace_id, source) in task_sources {
        if let TaskSource::Linear(target) = source {
            grouped
                .entry((target.project_id.clone(), target.labels.clone()))
                .or_default()
                .push((workspace_id.clone(), target.feature_name.clone()));
        }
    }
    grouped
}

pub async fn gather_refresh_snapshot_strict(
    client: &CmuxClient,
    histories_dir: &std::path::Path,
) -> Result<RefreshSnapshot> {
    gather_refresh_snapshot_inner(client, histories_dir, true).await
}

async fn gather_refresh_snapshot_inner(
    client: &CmuxClient,
    histories_dir: &std::path::Path,
    strict: bool,
) -> Result<RefreshSnapshot> {
    // Read only arcmux's local projection. The client performs three
    // concurrent GETs and never triggers a remote sync.
    let mesh_task = tokio::spawn(async {
        crate::mc_data::arcmux_mesh::ArcmuxMeshClient::default()
            .fetch()
            .await
    });
    let workspaces = client.list_workspaces().await?;
    let expected_window_ref = workspaces.iter().find_map(|ws| ws.window_ref.as_deref());
    let mut surfaces_reliable = true;
    let surfaces_map = match client.get_surfaces_for_window(expected_window_ref).await {
        Ok(map) => map,
        Err(e) if strict => return Err(e).context("get cmux surfaces"),
        Err(e) => {
            eprintln!("cmux surfaces refresh failed: {e:#}");
            surfaces_reliable = false;
            HashMap::new()
        }
    };
    let missing_workspace_refs: Vec<&str> = workspaces
        .iter()
        .filter(|ws| !surfaces_map.contains_key(&ws.ref_id))
        .map(|ws| ws.ref_id.as_str())
        .collect();
    if !missing_workspace_refs.is_empty() {
        surfaces_reliable = false;
        if strict {
            anyhow::bail!(
                "cmux tree missing workspaces from current window: {}",
                missing_workspace_refs.join(", ")
            );
        } else {
            eprintln!(
                "cmux tree missing workspaces from current window: {}",
                missing_workspace_refs.join(", ")
            );
        }
    }
    let surface_count: usize = surfaces_map.values().map(Vec::len).sum();
    if !workspaces.is_empty() && surface_count == 0 {
        surfaces_reliable = false;
        if strict {
            anyhow::bail!("cmux tree reported no surfaces for current window");
        } else {
            eprintln!("cmux tree reported no surfaces for current window");
        }
    }
    let histories_validation = crate::mc_data::session_log::validate_histories_dir(histories_dir);
    let histories_valid = match histories_validation {
        Ok(()) => true,
        Err(e) if strict => return Err(e).context("validate histories dir"),
        Err(e) => {
            eprintln!("histories dir validation failed: {e:#}");
            false
        }
    };

    // Set of UUIDs we still need to find sessions for. The parser loop
    // below exits as soon as every workspace has a hit, so on the typical
    // case we parse roughly one file per workspace instead of every recent
    // session log.
    let known_uuids: std::collections::HashSet<String> =
        workspaces.iter().map(|w| w.uuid.clone()).collect();

    let dir = histories_dir.to_path_buf();
    let dir_for_surface_sessions = histories_dir.to_path_buf();
    let workspaces_for_surface_sessions = workspaces.clone();
    let surfaces_map_for_surface_sessions = surfaces_map.clone();
    let sessions_task = tokio::task::spawn_blocking(move || {
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
    });
    let surface_sessions_task =
        tokio::task::spawn_blocking(move || {
            let mut out: HashMap<
                String,
                HashMap<String, crate::mc_data::window_registry::SurfaceSessionRecord>,
            > = HashMap::new();
            let host = hostname_short();
            // cmux's authoritative per-surface binding: surface UUID -> the
            // agent's native transcript. Read once; used as the top-priority
            // intent source so each surface shows ITS OWN overall/latest.
            let bound_by_surface = crate::mc_data::cmux_sessions::load_by_surface();
            // Persistent provider "overall" summaries keyed by transcript path; these
            // override the deterministic first-turn overall when present.
            let overall_cache = crate::mc_data::overall_cache::load();
            for ws in workspaces_for_surface_sessions {
                let surfaces = surfaces_map_for_surface_sessions
                    .get(&ws.ref_id)
                    .cloned()
                    .unwrap_or_default();
                let ctx = crate::mc_data::session_log::WorkspaceContext {
                    host: Some(host.clone()),
                    cwd: ws.current_directory.clone(),
                };
                let mut by_surface = HashMap::new();
                for (surface_idx, surface) in surfaces.iter().enumerate() {
                    let eff = crate::mc_data::surface_kind::effective_kind(
                        &ws.uuid,
                        &surface.ref_id,
                        surface.kind,
                    );
                    if !eff.is_agent() {
                        continue;
                    }
                    // Top priority: cmux binding → the agent's native transcript.
                    // Exact per surface, so two panes can't share a prompt and an
                    // unstarted/exited pane shows nothing of its own.
                    let bound = surface
                        .uuid
                        .as_deref()
                        .and_then(|id| bound_by_surface.get(id));
                    if let Some(b) = bound {
                        if let Some(tp) = b.transcript_path.as_deref() {
                            let mut intent =
                                crate::mc_data::transcript::intent_from_transcript(b.agent, tp);
                            // Prefer the persistent provider session summary for
                            // "overall" when one is cached for this transcript.
                            if let Some((summary, _)) =
                                overall_cache.get(&tp.to_string_lossy().into_owned())
                            {
                                intent.overall_goal = Some(summary.clone());
                            }
                            if intent.overall_goal.is_some() || intent.latest_ask.is_some() {
                                by_surface.insert(
                                    surface.ref_id.clone(),
                                    crate::mc_data::window_registry::SurfaceSessionRecord {
                                        path: tp.to_path_buf(),
                                        frontmatter: Default::default(),
                                        intent,
                                    },
                                );
                                continue; // binding wins; skip fuzzy markdown match
                            }
                        }
                    }
                    let same_agent_index = surfaces[..surface_idx]
                        .iter()
                        .filter(|prev| {
                            crate::mc_data::surface_kind::effective_kind(
                                &ws.uuid,
                                &prev.ref_id,
                                prev.kind,
                            ) == eff
                        })
                        .count();
                    let resolution =
                        crate::mc_data::session_log::resolve_session_log_for_surface_in_dir(
                            &dir_for_surface_sessions,
                            &ws.uuid,
                            &surface.ref_id,
                            &ctx,
                            Some(eff.label()),
                            same_agent_index,
                        )
                        .ok()
                        .flatten();
                    if let Some(resolution) = resolution {
                        let intent = read_intent_from_session_path(&resolution.path)
                            .unwrap_or_else(|| crate::mc_data::session_log::ConversationIntent {
                                overall_goal: None,
                                latest_ask: None,
                            });
                        by_surface.insert(
                            surface.ref_id.clone(),
                            crate::mc_data::window_registry::SurfaceSessionRecord {
                                path: resolution.path,
                                frontmatter: resolution.frontmatter,
                                intent,
                            },
                        );
                    }
                }
                if !by_surface.is_empty() {
                    out.insert(ws.uuid, by_surface);
                }
            }
            out
        });

    let sessions_by_ws_id = sessions_task.await.unwrap_or_default();
    let surface_sessions_by_ws_id = surface_sessions_task.await.unwrap_or_default();
    let registry_output = crate::mc_data::window_registry::build_registry(
        &workspaces,
        &surfaces_map,
        &surface_sessions_by_ws_id,
        histories_dir,
        histories_valid,
    );
    let registry_for_write = registry_output.registry.clone();
    if surfaces_reliable {
        if let Err(e) = tokio::task::spawn_blocking(move || {
            crate::mc_data::window_registry::write_registry(&registry_for_write)
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("registry writer join failed: {e}")))
        {
            if strict {
                return Err(e).context("write window registry");
            }
            eprintln!("window registry write failed: {e:#}");
        }
    } else {
        eprintln!("skipping window registry write because cmux surface snapshot was incomplete");
    }
    let repo_roots_by_ws_id = registry_output.repo_roots_by_ws_id;
    let repo_by_surface_by_ws_id = registry_output.repo_by_surface_by_ws_id;
    let project_registry = crate::mc_data::project_registry::ProjectRegistry::load_default().ok();
    let task_sources = resolve_task_sources(
        &workspaces,
        &repo_roots_by_ws_id,
        project_registry.as_ref(),
    );
    let task_sources_for_beads = task_sources.clone();
    let beads_by_ws_id = tokio::task::spawn_blocking(move || {
        use crate::mc_data::project_registry::TaskSource;
        let mut map = HashMap::new();
        for (ws_id, repo_roots) in repo_roots_by_ws_id {
            if matches!(
                task_sources_for_beads.get(&ws_id),
                Some(TaskSource::Linear(_) | TaskSource::LinearUnavailable)
            ) {
                continue;
            }
            let surface_map = repo_by_surface_by_ws_id
                .get(&ws_id)
                .cloned()
                .unwrap_or_default();
            if let Some(view) =
                crate::mc_data::beads::workspace_view_for_repos(&repo_roots, surface_map)
            {
                map.insert(ws_id, view);
            }
        }
        map
    })
    .await
    .unwrap_or_default();
    let linear_by_ws_id = gather_linear_views(&task_sources).await;
    let surface_intents_by_ws_id = surface_sessions_by_ws_id
        .into_iter()
        .map(|(ws_id, records)| {
            let intents = records
                .into_iter()
                .map(|(surface_ref, record)| (surface_ref, record.intent))
                .collect();
            (ws_id, intents)
        })
        .collect();
    let mesh_fetch = mesh_task
        .await
        .unwrap_or_else(|_| crate::mc_data::arcmux_mesh::MeshFetch {
            snapshot: None,
            warning: Some("arcmux mesh reader unavailable".to_string()),
        });

    Ok(RefreshSnapshot {
        workspaces,
        surfaces_map,
        sessions_by_ws_id,
        beads_by_ws_id,
        linear_by_ws_id,
        surface_intents_by_ws_id,
        remote_mesh: mesh_fetch.snapshot,
        mesh_warning: mesh_fetch.warning,
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
            global_warning: None,
            mesh_warning: None,
            session_to_workspace: HashMap::new(),
            workspace_index: HashMap::new(),
            bullet_hashes: HashMap::new(),
            beads_generation: 0,
            pending_dismissal: None,
            input_mode: crate::tui::command::InputMode::Normal,
            remote_watch: crate::mc_data::remote_intent::RemoteWatch::new(),
            remote_grab_tick: 0,
        }
    }

    pub async fn refresh_workspaces(
        &mut self,
        client: &CmuxClient,
        histories_dir: &std::path::Path,
        short_text_client: Option<&crate::llm::short_text::ShortTextClient>,
    ) -> Result<()> {
        let snap = gather_refresh_snapshot(client, histories_dir).await?;
        self.apply_refresh_snapshot(snap, short_text_client).await;
        Ok(())
    }

    pub fn beads_refresh_targets(&self) -> Vec<BeadsRefreshTarget> {
        self.workspaces
            .iter()
            .filter_map(|ws| {
                let beads = ws.beads.as_ref()?;
                Some(BeadsRefreshTarget {
                    generation: self.beads_generation,
                    workspace_id: ws.workspace.uuid.clone(),
                    repo_roots: beads
                        .repos
                        .iter()
                        .map(|repo| repo.repo_path.clone())
                        .collect(),
                    repo_by_surface_ref: beads.repo_by_surface_ref.clone(),
                })
            })
            .collect()
    }

    pub async fn apply_beads_refresh_snapshot(&mut self, snap: BeadsRefreshSnapshot) {
        self.apply_beads_refresh_snapshot_with_saver(snap, |uuid, stable_doc| {
            let traj_path = crate::mc_data::paths::trajectory_path(uuid);
            if let Err(e) = stable_doc.save_to_file(&traj_path) {
                eprintln!("save trajectory after beads refresh ({uuid}): {e:?}");
            }
        });
    }

    fn apply_beads_refresh_snapshot_with_saver<F>(
        &mut self,
        snap: BeadsRefreshSnapshot,
        mut save: F,
    ) where
        F: FnMut(&str, &crate::mc_data::trajectory::TrajectoryDoc),
    {
        if snap.generation != self.beads_generation {
            return;
        }
        for ws_state in self.workspaces.iter_mut() {
            let Some(beads) = snap.beads_by_ws_id.get(&ws_state.workspace.uuid).cloned() else {
                continue;
            };
            ws_state.beads = Some(beads.clone());

            let in_insert = ws_state
                .edit_state
                .as_ref()
                .map(|s| matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. }))
                .unwrap_or(false);
            if in_insert || ws_state.peek_state.is_some() {
                continue;
            }

            let highlighted_repo = highlighted_surface_id(ws_state)
                .and_then(|surface_id| beads.repo_by_surface_ref.get(&surface_id).cloned());
            let beads_items = beads_items_for_view(&beads, highlighted_repo.as_deref());
            let Some(doc) = ws_state.trajectory.as_mut() else {
                continue;
            };
            let existing_items = doc
                .section(crate::mc_data::trajectory::SECTION_GOALS)
                .map(|section| section.items.as_slice())
                .unwrap_or(&[]);
            if items_equal_for_projection(existing_items, &beads_items) {
                continue;
            }

            doc.replace_section_items(crate::mc_data::trajectory::SECTION_GOALS, beads_items);
            let persisted = crate::tui::trajectory_edit::stable_doc_for_persistence(
                doc,
                ws_state.edit_state.as_ref(),
            );
            save(&ws_state.workspace.uuid, &persisted);
        }
    }

    /// Apply a pre-gathered refresh snapshot to `self`. Pure mutation, no I/O
    /// that could block: the slow parts (cmux client calls, 999-file session
    /// parsing) ran off-thread in `gather_refresh_snapshot` and arrived here as
    /// data. Per-workspace file reads (trajectory.md, notes) are
    /// still synchronous but are bounded — ~25 workspaces × ~4 small files ≈
    /// 100 reads, which is ~tens of ms total on a warm cache.
    pub async fn apply_refresh_snapshot(
        &mut self,
        snap: RefreshSnapshot,
        short_text_client: Option<&crate::llm::short_text::ShortTextClient>,
    ) {
        let RefreshSnapshot {
            workspaces,
            surfaces_map,
            mut sessions_by_ws_id,
            mut beads_by_ws_id,
            mut linear_by_ws_id,
            surface_intents_by_ws_id,
            remote_mesh,
            mesh_warning,
        } = snap;
        self.mesh_warning = mesh_warning;
        self.beads_generation = self.beads_generation.wrapping_add(1);

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
        let old_linear_views: HashMap<
            String,
            Option<crate::mc_data::linear::WorkspaceLinearView>,
        > = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.linear.clone()))
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
        // Preserve mux-derived agent status across refreshes. The cmux event
        // stream is retained only to map session_id -> workspace_id; working
        // and turn-timing facts are polled from ~/data/mux/sessions/*.json.
        let old_mux_statuses: HashMap<String, Option<MuxSessionState>> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.mux_status.clone()))
            .collect();
        let old_remote_surfaces: HashMap<
            String,
            HashMap<String, crate::mc_data::arcmux_mesh::RemoteSurfaceState>,
        > = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.remote_surfaces.clone()))
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
                let mut linear = linear_by_ws_id.remove(&ws.uuid);
                if let Some(ref mut refreshed) = linear {
                    let previous = old_linear_views.get(&ws.uuid).and_then(Option::as_ref);
                    retain_last_good_linear(refreshed, previous);
                }
                let beads = if linear.is_some() {
                    None
                } else {
                    beads_by_ws_id.remove(&ws.uuid)
                };
                let surfaces = surfaces_map.get(&ws.ref_id).cloned().unwrap_or_default();
                let remote_surfaces = if let Some(mesh) = remote_mesh.as_ref() {
                    mesh.resolve_workspace(&ws.uuid, &surfaces)
                } else {
                    let old = old_remote_surfaces
                        .get(&ws.uuid)
                        .cloned()
                        .unwrap_or_default();
                    retain_remote_surfaces_as_stale(&old, &surfaces)
                };
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
                    let has_pending_mission_move = old_edit_states
                        .get(&ws.uuid)
                        .and_then(|s| s.as_ref())
                        .map(|s| s.has_pending_mission_moves())
                        .unwrap_or(false);
                    in_insert || in_peek || has_pending_mission_move
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
                let mux_status = old_mux_statuses.get(&ws.uuid).cloned().flatten();
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
                    remote_surfaces,
                    screen_preview,
                    screen_insights,
                    tool_call_count,
                    notes,
                    mux_status,
                    classification: None,
                    loading: false,
                    summary,
                    beads,
                    linear,
                    linear_open_pending: None,
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
                || ws_state
                    .edit_state
                    .as_ref()
                    .map(|s| s.has_pending_mission_moves())
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
            let goal_section_empty = doc.mission_history.is_empty()
                && doc
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
                    is_checkbox: true,
                    checked: Some(false),
                    surface_id: None,
                })
                .collect();
            if goal_items.is_empty() {
                continue;
            }
            doc.replace_section_items(crate::mc_data::trajectory::SECTION_MISSION, goal_items);
            let persisted = crate::tui::trajectory_edit::stable_doc_for_persistence(
                doc,
                ws_state.edit_state.as_ref(),
            );
            let traj_path = crate::mc_data::paths::trajectory_path(&ws_state.workspace.uuid);
            if let Err(e) = persisted.save_to_file(&traj_path) {
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
        // Track prefixes assigned earlier in this same refresh pass so two
        // workspaces processed back-to-back don't both pick the same code
        // (e.g. "MSC" for two repos that look alike).
        let mut used_prefixes_this_pass: Vec<String> = Vec::new();
        // Snapshot provider-inferred intents for remote surfaces before the mutable
        // workspace loop (can't borrow self.remote_watch inside iter_mut).
        let remote_intents = self.remote_watch.all_intents();
        for ws_state in self.workspaces.iter_mut() {
            let surface_intents_for_ws = surface_intents_by_ws_id.get(&ws_state.workspace.uuid);
            if ws_state
                .edit_state
                .as_ref()
                .map(|s| matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. }))
                .unwrap_or(false)
                || ws_state.peek_state.is_some()
            {
                continue;
            }
            let Some(ref doc) = ws_state.trajectory else {
                continue;
            };

            // Load goals.json. Mutable because we may populate `prefix` /
            // bump `next_seq` while assigning IDs to goal rows below.
            let mut goals = crate::mc_data::goals_json::GoalsFile::load(&ws_state.workspace.uuid);
            // Carry forward this workspace's prefix into the per-pass used
            // list (and stash it for later prefix-assignment, which still
            // needs to see the workspace's OWN prefix).
            if let Some(ref existing) = goals.prefix {
                if !used_prefixes_this_pass.iter().any(|p| p == existing) {
                    used_prefixes_this_pass.push(existing.clone());
                }
            }

            // Build the new item list from the surfaces vec. The stable cmux
            // surface ref is retained as surface_id so bound remote peeks read
            // that exact local surface rather than a workspace-local transcript.
            let surface_items: Vec<crate::mc_data::trajectory::Item> = ws_state
                .surfaces
                .iter()
                .filter_map(|s| {
                    // `effective_kind` keeps the agent glyph for ~5 min after
                    // the agent exits (Shell/Unknown current + recent
                    // last-agent file ⇒ surface the agent kind instead).
                    let remote = ws_state.remote_surfaces.get(&s.ref_id);
                    if !remote_surface_is_current(remote) {
                        return None;
                    }
                    let eff = remote.map(|state| state.surface_kind()).unwrap_or_else(|| {
                        crate::mc_data::surface_kind::effective_kind(
                            &ws_state.workspace.uuid,
                            &s.ref_id,
                            s.kind,
                        )
                    });
                    let title = remote
                        .map(|state| state.stable_title(&s.title))
                        .unwrap_or_else(|| s.title.clone());
                    // Remote (mosh/ssh) surfaces have no local session log; their
                    // overall/latest comes from provider inference over the
                    // screen-grab transcript (see remote_intent). Local agent
                    // surfaces use the session-log/screen path as before.
                    // Bound surfaces already carry the provider "overall" summary
                    // (applied in the gather phase from overall_cache); the
                    // workspace-fallback path is for unbound surfaces only.
                    let intent = if remote.is_some() {
                        None
                    } else if eff == crate::mc_data::surface_kind::SurfaceKind::Remote {
                        remote_intents.get(&s.ref_id).cloned()
                    } else {
                        surface_intent_summary(
                            surface_intents_for_ws.and_then(|m| m.get(&s.ref_id)),
                            ws_state.session.as_ref(),
                            ws_state.screen_insights.user_prompt.as_deref(),
                            s,
                            eff,
                            &goals,
                        )
                    };
                    let text = crate::mc_data::surface_render::format_surface_text(
                        eff,
                        &title,
                        &goals,
                        &s.ref_id,
                        intent.as_ref(),
                    );
                    Some(crate::mc_data::trajectory::Item {
                        text,
                        is_checkbox: false,
                        checked: None,
                        // Use the surface's own ref_id (e.g. "surface:92") so that
                        // peek mode can distinguish surfaces within the same workspace
                        // and distribute session logs deterministically by index.
                        surface_id: Some(s.ref_id.clone()),
                    })
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

            // Replace the canonical task section with the registry-selected
            // projection. Linear is authoritative over any stale `.beads/`;
            // the persisted section name stays `Beads` for compatibility and
            // the renderer supplies the visible `Linear` title.
            let goals_section_existing = doc
                .section(crate::mc_data::trajectory::SECTION_GOALS)
                .map(|s| s.items.clone())
                .unwrap_or_default();
            let mut goals_mutated = false;
            let (goals_unchanged, goals_items_opt) = if let Some(linear) = ws_state.linear.as_ref() {
                let linear_items = linear_items_for_view(linear);
                let unchanged =
                    items_equal_for_projection(&goals_section_existing, &linear_items);
                (unchanged, Some(linear_items))
            } else if let Some(beads) = ws_state.beads.as_ref() {
                let highlighted_repo = highlighted_surface_repo(ws_state);
                let beads_items = beads_items_for_view(beads, highlighted_repo.as_deref());
                let unchanged = items_equal_for_projection(&goals_section_existing, &beads_items);
                (unchanged, Some(beads_items))
            } else {
                // Strip mc-injected bead rows left over from a repo a surface no
                // longer resolves to (e.g. gmail-triage kept showing elonco's
                // beads after its codex left the elonco checkout). They are not
                // this project's goals; drop them so the section clears rather
                // than showing another project's issues.
                let (goals_section_existing, dropped_task_rows) =
                    strip_projected_task_rows(&goals_section_existing);
                let any_existing_badge = goals_section_existing
                    .iter()
                    .any(|i| i.text.contains("   → "));
                let any_missing_id = goals_section_existing.iter().any(|i| {
                    !i.text.trim().is_empty() && !crate::mc_data::goals_json::has_id_prefix(&i.text)
                });
                // Generate a prefix exactly ONCE per workspace — at the moment
                // the workspace earns its first non-blank legacy goal row.
                let needs_prefix = goals.prefix.is_none()
                    && goals_section_existing
                        .iter()
                        .any(|i| !i.text.trim().is_empty());
                if needs_prefix {
                    let workspace_name = ws_state.workspace.name.clone();
                    let picked: String = match short_text_client {
                        Some(client) => {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                crate::llm::short_text::generate_workspace_prefix(
                                    client,
                                    &workspace_name,
                                    &used_prefixes_this_pass,
                                ),
                            )
                            .await
                            {
                                Ok(Ok(p)) => p,
                                Ok(Err(e)) => {
                                    eprintln!(
                                        "{} prefix gen failed for {}: {e:#}; using deterministic.",
                                        client.provider_name(),
                                        workspace_name
                                    );
                                    crate::llm::short_text::deterministic_prefix(
                                        &workspace_name,
                                        &used_prefixes_this_pass,
                                    )
                                }
                                Err(_) => {
                                    eprintln!(
                                        "{} prefix gen timed out for {}; using deterministic.",
                                        client.provider_name(),
                                        workspace_name
                                    );
                                    crate::llm::short_text::deterministic_prefix(
                                        &workspace_name,
                                        &used_prefixes_this_pass,
                                    )
                                }
                            }
                        }
                        None => crate::llm::short_text::deterministic_prefix(
                            &workspace_name,
                            &used_prefixes_this_pass,
                        ),
                    };
                    goals.prefix = Some(picked.clone());
                    used_prefixes_this_pass.push(picked);
                }
                let goals_need_rebuild = !goals.goals.is_empty()
                    || any_existing_badge
                    || (goals.prefix.is_some() && any_missing_id);
                goals_mutated = needs_prefix;

                if goals_need_rebuild {
                    let rebuilt: Vec<crate::mc_data::trajectory::Item> = goals_section_existing
                        .iter()
                        .map(|i| {
                            let no_badge = crate::mc_data::surface_render::strip_badge(&i.text);
                            let (raw, existing_id) =
                                crate::mc_data::goals_json::strip_id_prefix(no_badge);
                            let id_to_use: Option<String> = if raw.trim().is_empty() {
                                None
                            } else if let Some(eid) = existing_id {
                                Some(eid.to_string())
                            } else if goals.prefix.is_some() {
                                goals_mutated = true;
                                goals.allocate_id()
                            } else {
                                None
                            };
                            let mut text = match &id_to_use {
                                Some(id) => format!("[{}] {}", id, raw),
                                None => raw.to_string(),
                            };
                            if let Some(badge) =
                                crate::mc_data::surface_render::format_goal_badge(&goals, raw)
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
                    let unchanged = items_equal_for_projection(&goals_section_existing, &rebuilt);
                    (unchanged, Some(rebuilt))
                } else if dropped_task_rows {
                    // Removed stale projected rows with nothing to rebuild —
                    // write the cleaned (goals-only, possibly empty) section.
                    (false, Some(goals_section_existing))
                } else {
                    (true, None)
                }
            };

            // Persist goals.json if we set a prefix or allocated any IDs.
            if goals_mutated {
                if let Err(e) = goals.save(&ws_state.workspace.uuid) {
                    eprintln!("goals.json save ({}): {e:?}", ws_state.workspace.uuid);
                }
            }

            if surfaces_unchanged && goals_unchanged {
                continue;
            }

            let doc = ws_state.trajectory.as_mut().expect("checked above");
            doc.replace_section_items(
                crate::mc_data::trajectory::SECTION_CURRENT_SURFACES,
                surface_items,
            );
            if let Some(items) = goals_items_opt {
                doc.replace_section_items(crate::mc_data::trajectory::SECTION_GOALS, items);
            }

            let persisted = crate::tui::trajectory_edit::stable_doc_for_persistence(
                doc,
                ws_state.edit_state.as_ref(),
            );
            let traj_path = crate::mc_data::paths::trajectory_path(&ws_state.workspace.uuid);
            if let Err(e) = persisted.save_to_file(&traj_path) {
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
        // Retained event names feed non-status consumers such as debugging and
        // future filtering, but working/waiting is read from mux JSON docs.
        let _ = event.event_name.as_str();

        if let Some(&idx) = self.workspace_index.get(&event.workspace_id) {
            self.workspaces[idx].tool_call_count += 1;
        }
    }

    pub fn refresh_mux_statuses_from_disk(&mut self) {
        let dir = crate::mc_data::mux_state::session_state_dir();
        self.refresh_mux_statuses_from_dir(&dir);
    }

    pub fn refresh_mux_statuses_from_dir(&mut self, dir: &std::path::Path) {
        if !dir.exists() {
            return;
        }

        let mut states = Vec::new();
        let mut active_by_id: HashMap<String, MuxSessionState> = crate::mc_data::mux_state::load_all_in_dir(dir)
            .into_iter()
            .map(|state| (state.session_id.clone(), state))
            .collect();
        for session_id in self.session_to_workspace.keys() {
            if let Some(state) = active_by_id.remove(session_id) {
                states.push(state);
                continue;
            }
            match crate::mc_data::mux_state::load_session_in_dir(dir, session_id) {
                Ok(Some(state)) => states.push(state),
                Ok(None) => {}
                Err(e) => eprintln!("load mux session state {session_id}: {e:#}"),
            }
        }
        self.apply_mux_session_states(states);
    }

    pub fn apply_mux_session_states<I>(&mut self, states: I)
    where
        I: IntoIterator<Item = MuxSessionState>,
    {
        let by_session: HashMap<String, MuxSessionState> = states
            .into_iter()
            .map(|state| (state.session_id.clone(), state))
            .collect();
        let mut by_workspace: HashMap<String, MuxSessionState> = HashMap::new();

        for (session_id, workspace_id) in &self.session_to_workspace {
            let Some(state) = by_session.get(session_id).cloned() else {
                continue;
            };
            match by_workspace.get(workspace_id) {
                Some(existing) if existing.updated_at >= state.updated_at => {}
                _ => {
                    by_workspace.insert(workspace_id.clone(), state);
                }
            }
        }

        for ws in &mut self.workspaces {
            if self
                .session_to_workspace
                .values()
                .any(|workspace_id| workspace_id == &ws.workspace.uuid)
            {
                ws.mux_status = by_workspace.remove(&ws.workspace.uuid);
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

    pub fn bottom_info(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(ws) = self.selected_workspace() {
            if self.focus == Focus::Detail {
                if let Some(surface_id) = highlighted_surface_id(ws) {
                    parts.push(format!("surface {surface_id}"));
                }
            }
            parts.push(format!("workspace {}", ws.workspace.uuid));
        }
        if let Some(window_id) = self
            .workspaces
            .iter()
            .find_map(|ws| ws.workspace.window_id.as_deref())
        {
            parts.push(format!("window {window_id}"));
        }
        // Put the warning last: the info renderer truncates from the left, so
        // narrow terminals retain the actionable warning instead of the IDs.
        if let Some(warning) = self.global_warning.as_deref() {
            parts.push(format!("⚠ {warning}"));
        }
        if let Some(warning) = self.mesh_warning.as_deref() {
            parts.push(format!("⚠ {warning}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
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
            // TypeSafe only earns its keep on remote workspaces; local
            // activity is read from mux state, then screen-regex fallbacks.
            let classifier = if ws.needs_remote_screen_inference() {
                classifier
            } else {
                None
            };
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
            // Remote-only: skip the TypeSafe call for local mux/screen-covered workspaces.
            let ws_classifier = if ws.needs_remote_screen_inference() {
                classifier.clone()
            } else {
                None
            };
            spawn_screen_task(
                ws.workspace.uuid.clone(),
                ws.workspace.ref_id.clone(),
                client.clone(),
                ws_classifier,
                tx.clone(),
            );
        }
    }

    /// Fire off background captures for every remote (mosh/ssh) surface that's
    /// due this tick, feeding results back via `tx`. Detection (`ps -A`) and the
    /// captures run off the main loop so the UI never blocks.
    ///
    /// Phase 2: this maintains the per-surface frame mergers + a debug
    /// transcript dump. Rendering the inferred two lines is wired in phase 3.
    pub fn spawn_remote_grabs(
        &mut self,
        client: CmuxClient,
        tx: tokio::sync::mpsc::UnboundedSender<RemoteGrabUpdate>,
    ) {
        self.remote_grab_tick = self.remote_grab_tick.wrapping_add(1);
        let tick = self.remote_grab_tick;

        // (workspace_uuid, surface_ref, tty) for every surface that has a tty
        // and is due this tick (backoff). Non-remote surfaces are filtered out
        // in the task once detection runs.
        let mut candidates: Vec<(String, String, String)> = Vec::new();
        let mut live_refs: Vec<String> = Vec::new();
        for ws in &self.workspaces {
            for s in &ws.surfaces {
                live_refs.push(s.ref_id.clone());
                // An exact mesh binding is authoritative. Do not feed its
                // local terminal capture into the legacy inference provider.
                if ws.remote_surfaces.contains_key(&s.ref_id) {
                    continue;
                }
                if let Some(tty) = &s.tty {
                    if self.remote_watch.due(&s.ref_id, tick) {
                        candidates.push((ws.workspace.uuid.clone(), s.ref_id.clone(), tty.clone()));
                    }
                }
            }
        }
        // Drop mergers for surfaces that no longer exist.
        self.remote_watch.retain(&live_refs);

        if candidates.is_empty() {
            return;
        }

        tokio::spawn(async move {
            use tokio::time::{Duration, timeout};
            // Detect which candidates are remote (mosh/ssh) in one `ps -A`.
            let ttys: Vec<&str> = candidates.iter().map(|(_, _, t)| t.as_str()).collect();
            let remote = crate::mc_data::surface_kind::detect_remote_all(&ttys);

            for (ws_uuid, surface_ref, tty) in &candidates {
                if remote.get(tty.as_str()).copied() != Some(true) {
                    continue;
                }
                let raw = timeout(
                    Duration::from_secs(4),
                    client.read_surface_text(surface_ref, 200),
                )
                .await
                .ok()
                .and_then(|r| r.ok())
                .filter(|s| !s.trim().is_empty());
                if let Some(raw) = raw {
                    let _ = tx.send(RemoteGrabUpdate {
                        workspace_uuid: ws_uuid.clone(),
                        surface_ref: surface_ref.clone(),
                        raw,
                    });
                }
            }
        });
    }

    /// Apply a remote-surface capture: feed the frame merger (dedup + status
    /// peel) and persist the debug transcript. Returns
    /// `Some((workspace_uuid, surface_ref, transcript))` when the transcript has
    /// grown enough to warrant fresh provider intent inference (change-gated).
    pub fn apply_remote_grab(
        &mut self,
        update: RemoteGrabUpdate,
    ) -> Option<(String, String, String)> {
        self.remote_watch
            .apply(&update.workspace_uuid, &update.surface_ref, &update.raw);
        self.remote_watch
            .transcript_for_inference(&update.surface_ref)
            .map(|transcript| (update.workspace_uuid, update.surface_ref, transcript))
    }

    /// Store a provider-inferred intent for a remote surface (rendered next refresh).
    pub fn apply_remote_intent(&mut self, update: RemoteIntentUpdate) {
        self.remote_watch
            .set_intent(&update.surface_ref, update.intent);
    }

    /// When the detail panel is open on a workspace, generate (or refresh) the
    /// selected-provider "overall" session summary for each bound agent surface.
    /// Change-gated: regenerate only when a surface has ≥`OVERALL_SUMMARY_EVERY`
    /// new user turns since its last summary (or has none yet). Runs off the
    /// main loop; results return via [`App::apply_overall_summary`].
    pub fn spawn_overall_summaries(
        &self,
        short_text_client: crate::llm::short_text::ShortTextClient,
    ) {
        if self.focus != Focus::Detail {
            return;
        }
        let Some(ws) = self.selected_workspace() else {
            return;
        };
        // (agent, transcript_path) for each bound agent surface in this workspace.
        let mut targets: Vec<(
            crate::mc_data::surface_kind::SurfaceKind,
            std::path::PathBuf,
        )> = Vec::new();
        let bound = crate::mc_data::cmux_sessions::load_by_surface();
        for s in &ws.surfaces {
            let eff =
                crate::mc_data::surface_kind::effective_kind(&ws.workspace.uuid, &s.ref_id, s.kind);
            if !eff.is_agent() {
                continue;
            }
            let Some(uuid) = s.uuid.as_deref() else {
                continue;
            };
            let Some(b) = bound.get(uuid) else { continue };
            if let Some(tp) = b.transcript_path.as_ref() {
                targets.push((b.agent, tp.clone()));
            }
        }
        if targets.is_empty() {
            return;
        }
        tokio::spawn(async move {
            for (agent, transcript) in targets {
                let users = crate::mc_data::transcript::user_turns(agent, &transcript);
                let turns = users.len();
                if turns == 0 {
                    continue;
                }
                // Change gate via the persistent cache: regenerate only when the
                // session has advanced >= K user turns since the cached summary.
                let cached_turns = crate::mc_data::overall_cache::get(&transcript)
                    .map(|(_, n)| n)
                    .unwrap_or(0);
                let due = cached_turns == 0
                    || turns.saturating_sub(cached_turns) >= OVERALL_SUMMARY_EVERY;
                if !due {
                    continue;
                }
                if let Ok(summary) =
                    crate::llm::short_text::summarize_overall(&short_text_client, &users).await
                {
                    let _ = crate::mc_data::overall_cache::put(&transcript, &summary, turns);
                }
            }
        });
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
            .map(|s| {
                matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. })
                    || s.has_pending_mission_moves()
            })
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
    /// - Mission and Mission history are both empty, OR
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
                    .map(|s| {
                        matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. })
                            || s.has_pending_mission_moves()
                    })
                    .unwrap_or(false);
                if is_editing {
                    return false;
                }
                // Don't spawn another if one is already running
                if ws.regen.regen_in_flight {
                    return false;
                }
                // A completely blank Mission is itself a regen signal. A
                // workspace with completed Mission history is intentionally
                // allowed to have no active row; do not reopen finished work.
                let mission_is_empty = ws
                    .trajectory
                    .as_ref()
                    .map(|doc| {
                        doc.mission_history.is_empty()
                            && doc
                                .section(crate::mc_data::trajectory::SECTION_MISSION)
                                .map(|section| section.items.is_empty())
                                .unwrap_or(true)
                    })
                    .unwrap_or(true);
                if mission_is_empty {
                    return true;
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

        // Canonical user ask from ~/agents/histories/<file>.md (last `## boyan` block).
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
            .map(|s| {
                matches!(s.mode, crate::tui::trajectory_edit::EditMode::Insert { .. })
                    || s.has_pending_mission_moves()
            })
            .unwrap_or(false);
        if is_editing {
            // Clear in-flight flag so the next tick can retry.
            self.workspaces[idx].regen.regen_in_flight = false;
            return;
        }

        // Registry/API-backed task rows are projections, not model-owned
        // trajectory content. Preserve the live source across regen so a
        // generated task list cannot briefly replace read-only Linear/Beads.
        let projected_task_items = (self.workspaces[idx].linear.is_some()
            || self.workspaces[idx].beads.is_some())
            .then(|| {
                self.workspaces[idx]
                    .trajectory
                    .as_ref()
                    .and_then(|current| {
                        current
                            .section(crate::mc_data::trajectory::SECTION_GOALS)
                            .map(|section| section.items.clone())
                    })
                    .unwrap_or_default()
            });

        // Ensure canonical sections exist.
        doc.ensure_sections();
        if let Some(items) = projected_task_items {
            doc.replace_section_items(crate::mc_data::trajectory::SECTION_GOALS, items);
        }

        // Sort Beads if >10 items.
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
                let _outcome = ws.dispatch_modal.as_mut().map(|m| m.handle_key(key));
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
                    // among same-kind surfaces over the workspace's flat surface
                    // list (cmux's `index_in_pane` is per-pane so two panes can
                    // both have idx=0; we don't use it here).
                    let surface_id_for_lookup =
                        item.and_then(|i| i.surface_id.as_deref()).unwrap_or("");
                    let this_surface = ws
                        .surfaces
                        .iter()
                        .find(|s| s.ref_id == surface_id_for_lookup);
                    let raw_kind = this_surface.map(|s| s.kind).unwrap_or_default();
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
                    let source = if ws.remote_surfaces.contains_key(surface_id_for_lookup) {
                        // F10/F11/F12: a bound remote row remains the exact local
                        // cmux surface. Never resolve it to a local session.md.
                        crate::tui::peek_view::PeekSource::Shell
                    } else if surface_kind.is_agent() {
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

        // ── Nav mode + Linear row ────────────────────────────────────────
        // Projected Linear rows are read-only. Enter is the sole action and
        // only succeeds for a row that exactly matches a validated API issue.
        use crate::mc_data::trajectory::SECTION_GOALS;
        if let Some(linear) = ws.linear.as_ref()
            && matches!(state.mode, crate::tui::trajectory_edit::EditMode::Nav)
            && doc
                .sections
                .get(state.cursor_section)
                .map(|sec| sec.name == SECTION_GOALS)
                .unwrap_or(false)
        {
            match key.code {
                KeyCode::Enter => {
                    ws.linear_open_pending = doc
                        .sections
                        .get(state.cursor_section)
                        .and_then(|section| section.items.get(state.cursor_item))
                        .and_then(|item| linear_desktop_url_for_row(linear, &item.text));
                    state.pending_d_at = None;
                    return vec![];
                }
                KeyCode::Char(' ')
                | KeyCode::Char('x')
                | KeyCode::Char('X')
                | KeyCode::Char('d')
                | KeyCode::Char('o')
                | KeyCode::Char('O')
                | KeyCode::Char('i')
                | KeyCode::Char('J')
                | KeyCode::Char('K') => {
                    state.pending_d_at = None;
                    return vec![];
                }
                _ => {}
            }
        }

        // ── Nav mode + Beads row ─────────────────────────────────────────
        // Live Beads rows are projected from bd data and are read-only here;
        // legacy local rows still fall through to the dispatch/edit behavior.
        if ws.beads.is_some()
            && matches!(state.mode, crate::tui::trajectory_edit::EditMode::Nav)
            && doc
                .sections
                .get(state.cursor_section)
                .map(|sec| sec.name == SECTION_GOALS)
                .unwrap_or(false)
        {
            match key.code {
                KeyCode::Enter
                | KeyCode::Char(' ')
                | KeyCode::Char('x')
                | KeyCode::Char('X')
                | KeyCode::Char('d')
                | KeyCode::Char('o')
                | KeyCode::Char('O')
                | KeyCode::Char('i') => {
                    state.pending_d_at = None;
                    return vec![];
                }
                _ => {}
            }
        }

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
                            ws.dispatch_modal =
                                Some(crate::tui::dispatch_modal::DispatchModal::new(
                                    goal_text,
                                    workspace_uuid,
                                    workspace_ref,
                                    &surfaces,
                                ));
                            return vec![];
                        }
                    }
                }
            }
        }

        crate::tui::trajectory_edit::handle_key(state, doc, key)
    }

    /// Read and clear the exact Linear deep link requested by the selected row.
    pub fn take_linear_open_request(&mut self) -> Option<String> {
        self.workspaces
            .get_mut(self.selected)?
            .linear_open_pending
            .take()
    }

    /// Read and clear the pending dispatch outcome for the selected workspace.
    /// The main loop calls this after each key dispatch and acts on the
    /// outcome (running cmux commands, updating goals.json, closing the modal).
    pub fn take_dispatch_outcome(&mut self) -> Option<crate::tui::dispatch_modal::DispatchOutcome> {
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
    /// the selected workspace. Clears the flag and returns
    /// `(workspace_ref, surface_ref)`: select the workspace, then focus the
    /// surface so a split-pane workspace lands on the RIGHT pane (not just the
    /// workspace's last-focused pane). `surface_ref` equals `workspace_ref` when
    /// the peek had no specific surface — the caller skips the surface focus then.
    pub fn take_peek_yield(&mut self) -> Option<(String, String)> {
        let idx = self.selected;
        let ws = self.workspaces.get_mut(idx)?;
        if ws.peek_yield_pending {
            ws.peek_yield_pending = false;
            // After yielding, clear peek state (the user is going to work there).
            let (ws_ref, surface_ref) = ws
                .peek_state
                .as_ref()
                .map(|p| (p.workspace_ref.clone(), p.surface_ref.clone()))
                .unwrap_or_else(|| (ws.workspace.ref_id.clone(), ws.workspace.ref_id.clone()));
            ws.peek_state = None;
            Some((ws_ref, surface_ref))
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

    /// Commit due mission checkbox relocations for every workspace. Each row
    /// stays in its original section for the five-second grace period; this is
    /// the only periodic path that crosses it into Mission History or Mission.
    ///
    /// Returns `(workspace_ref, active_mission_text)` pairs so the event loop
    /// can mirror the settled Mission back to cmux without blocking the tick.
    pub fn settle_pending_mission_moves_at(&mut self, now: Instant) -> Vec<(String, String)> {
        self.settle_pending_mission_moves_with_saver(now, |uuid, doc, state, actions| {
            crate::tui::trajectory_edit::save(uuid, doc, state, actions).map(|_| ())
        })
    }

    fn settle_pending_mission_moves_with_saver<F>(
        &mut self,
        now: Instant,
        mut save: F,
    ) -> Vec<(String, String)>
    where
        F: FnMut(
            &str,
            &mut crate::mc_data::trajectory::TrajectoryDoc,
            &crate::tui::trajectory_edit::TrajectoryEditState,
            &[crate::tui::trajectory_edit::EditAction],
        ) -> anyhow::Result<()>,
    {
        let mut settled = Vec::new();
        for ws in &mut self.workspaces {
            let (Some(doc), Some(state)) = (ws.trajectory.as_mut(), ws.edit_state.as_mut()) else {
                continue;
            };
            // Settle copy-on-write: the live preview and pending timer remain
            // intact unless every persistence step succeeds.
            let mut next_doc = doc.clone();
            let mut next_state = state.clone();
            let actions = crate::tui::trajectory_edit::settle_pending_mission_moves_at(
                &mut next_state,
                &mut next_doc,
                now,
            );
            if actions.is_empty() {
                continue;
            }

            next_doc.sort_tasks_if_long();
            if let Err(error) = save(
                &ws.workspace.uuid,
                &mut next_doc,
                &next_state,
                &actions,
            ) {
                eprintln!(
                    "settle_pending_mission_moves({}): {error:?}",
                    ws.workspace.uuid
                );
                continue;
            }

            let description = next_doc
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .map(|section| {
                    section
                        .items
                        .iter()
                        .map(|item| item.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            *doc = next_doc;
            *state = next_state;
            settled.push((ws.workspace.ref_id.clone(), description));
        }
        settled
    }

    pub fn settle_pending_mission_moves(&mut self) -> Vec<(String, String)> {
        self.settle_pending_mission_moves_at(Instant::now())
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

fn highlighted_surface_repo(ws_state: &WorkspaceState) -> Option<PathBuf> {
    let surface_id = highlighted_surface_id(ws_state)?;
    ws_state
        .beads
        .as_ref()?
        .repo_by_surface_ref
        .get(&surface_id)
        .cloned()
}

fn highlighted_surface_id(ws_state: &WorkspaceState) -> Option<String> {
    let doc = ws_state.trajectory.as_ref()?;
    let state = ws_state.edit_state.as_ref()?;
    let section = doc.sections.get(state.cursor_section)?;
    if section.name != crate::mc_data::trajectory::SECTION_CURRENT_SURFACES {
        return None;
    }
    section.items.get(state.cursor_item)?.surface_id.clone()
}

fn retain_last_good_linear(
    refreshed: &mut crate::mc_data::linear::WorkspaceLinearView,
    previous: Option<&crate::mc_data::linear::WorkspaceLinearView>,
) {
    let Some(previous) = previous else {
        return;
    };
    if refreshed.warning.is_some()
        && refreshed.issues.is_empty()
        && refreshed.project_id == previous.project_id
        && refreshed.required_labels == previous.required_labels
        && refreshed.feature_name == previous.feature_name
        && !previous.issues.is_empty()
    {
        refreshed.issues.clone_from(&previous.issues);
    }
}

fn linear_items_for_view(
    view: &crate::mc_data::linear::WorkspaceLinearView,
) -> Vec<crate::mc_data::trajectory::Item> {
    let mut items = Vec::new();
    if let Some(feature_name) = view.feature_name.as_deref() {
        items.push(crate::mc_data::trajectory::Item {
            text: format!("feature: {feature_name}"),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
    }

    if view.issues.is_empty() {
        let text = view
            .warning
            .as_deref()
            .map(|warning| format!("  ({warning})"))
            .unwrap_or_else(|| "No active Linear issues".to_string());
        items.push(crate::mc_data::trajectory::Item {
            text,
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
        return items;
    }

    items.extend(
        view.issues
            .iter()
            .map(|issue| crate::mc_data::trajectory::Item {
                text: linear_issue_line(issue),
                is_checkbox: false,
                checked: None,
                surface_id: None,
            }),
    );
    if view.warning.is_some() {
        items.push(crate::mc_data::trajectory::Item {
            text: "  (stale — Linear refresh unavailable)".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });
    }
    items
}

fn linear_issue_line(issue: &crate::mc_data::linear::LinearIssue) -> String {
    let title = crate::mc_data::beads::compact_issue_title(&issue.title, 74);
    let state = issue.state_name.replace('_', "-");
    format!(
        "[{}] {} {} · {}",
        issue.priority_label(),
        issue.identifier,
        state,
        title
    )
}

fn linear_desktop_url_for_row(
    view: &crate::mc_data::linear::WorkspaceLinearView,
    row: &str,
) -> Option<String> {
    view.issues
        .iter()
        .find(|issue| linear_issue_line(issue) == row)
        .and_then(|issue| view.desktop_url_for_issue(&issue.identifier))
}

fn beads_items_for_view(
    view: &crate::mc_data::beads::WorkspaceBeadsView,
    highlighted_repo: Option<&std::path::Path>,
) -> Vec<crate::mc_data::trajectory::Item> {
    if view.repos.is_empty() {
        return vec![crate::mc_data::trajectory::Item {
            text: "No active beads".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        }];
    }

    let mut repo_indices: Vec<usize> = (0..view.repos.len()).collect();
    if let Some(highlighted_repo) = highlighted_repo {
        if let Some(pos) = repo_indices
            .iter()
            .position(|idx| view.repos[*idx].repo_path == highlighted_repo)
        {
            let idx = repo_indices.remove(pos);
            repo_indices.insert(0, idx);
        }
    }

    let mut items = Vec::new();
    for idx in repo_indices {
        let repo = &view.repos[idx];
        // Always show the repo header before its list (single- or multi-repo),
        // so the Beads section is unambiguous about which repo it reflects.
        items.push(crate::mc_data::trajectory::Item {
            text: format!("repo: {}", repo_display_name(&repo.repo_path)),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });

        if repo.issues.is_empty() {
            // Repo header already names the repo; keep the empty note short.
            let text = match repo.source {
                crate::mc_data::beads::BeadsSource::Unavailable => {
                    "  (beads unavailable — bd db empty and no issues.jsonl)".to_string()
                }
                _ => "  (no active beads)".to_string(),
            };
            items.push(crate::mc_data::trajectory::Item {
                text,
                is_checkbox: false,
                checked: None,
                surface_id: None,
            });
            continue;
        }

        items.extend(
            repo.issues
                .iter()
                .map(|issue| crate::mc_data::trajectory::Item {
                    text: bead_issue_line(issue),
                    is_checkbox: true,
                    checked: Some(issue.is_closed()),
                    surface_id: None,
                }),
        );
    }
    items
}

fn repo_display_name(repo_path: &std::path::Path) -> &str {
    repo_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
}

/// Does this `## Beads` section line look like an mc-injected bead row (a real
/// issue `[P0] id status · title`, or a `repo:` / `No active beads` / `Beads
/// unavailable` header) rather than a user/legacy goal? Used to drop stale
/// beads when a workspace no longer resolves to a beads repo.
fn is_projected_task_row(text: &str) -> bool {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("[P") {
        if let Some((label, _)) = rest.split_once("] ") {
            if !label.is_empty() && label.chars().all(|c| c.is_ascii_digit() || c == '?') {
                return true;
            }
        }
    }
    t.starts_with("repo: ")
        || t.starts_with("feature: ")
        || t.starts_with("No active beads")
        || t.starts_with("Beads unavailable")
        || t.starts_with("(no active beads)")
        || t.starts_with("(beads unavailable")
        || t.starts_with("No active Linear issues")
        || t.starts_with("(Linear unavailable")
        || t.starts_with("(stale — Linear refresh unavailable)")
}

fn strip_projected_task_rows(
    items: &[crate::mc_data::trajectory::Item],
) -> (Vec<crate::mc_data::trajectory::Item>, bool) {
    let filtered: Vec<_> = items
        .iter()
        .filter(|item| !is_projected_task_row(&item.text))
        .cloned()
        .collect();
    let dropped = filtered.len() != items.len();
    (filtered, dropped)
}

fn bead_issue_line(issue: &crate::mc_data::beads::BeadIssue) -> String {
    let title = crate::mc_data::beads::compact_issue_title(&issue.title, 74);
    let status = issue.status.replace('_', "-");
    format!(
        "[{}] {} {} · {}",
        issue.priority_label(),
        issue.id,
        status,
        title
    )
}

fn items_equal_for_projection(
    a: &[crate::mc_data::trajectory::Item],
    b: &[crate::mc_data::trajectory::Item],
) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(left, right)| {
            left.text == right.text
                && left.is_checkbox == right.is_checkbox
                && left.checked == right.checked
                && left.surface_id == right.surface_id
        })
}

fn surface_intent_summary(
    resolved_intent: Option<&crate::mc_data::session_log::ConversationIntent>,
    workspace_session: Option<&SessionFile>,
    screen_latest_ask: Option<&str>,
    surface: &SurfaceInfo,
    effective_kind: crate::mc_data::surface_kind::SurfaceKind,
    goals: &crate::mc_data::goals_json::GoalsFile,
) -> Option<crate::mc_data::surface_render::SurfaceIntentSummary> {
    if !effective_kind.is_agent() {
        return None;
    }

    // The workspace-level session + on-screen prompt are scraped from the
    // workspace's FOCUSED pane, so they describe exactly one surface. Only that
    // focused surface may borrow them — otherwise every agent surface in the
    // workspace (including exited/never-started panes kept alive by
    // effective_kind) would render the same prompt. A non-focused surface shows
    // intent only from its OWN resolved session.
    let mut intent = resolved_intent.cloned();

    if intent.is_none() && surface.focused {
        intent = workspace_session.and_then(|session| read_intent_from_session_path(&session.path));
    }

    let mut intent = intent.unwrap_or_default();
    if intent.overall_goal.is_none() {
        if let Some(goal) = goals.open_for_surface(&surface.ref_id).first() {
            intent.overall_goal = crate::mc_data::session_log::summarize_user_turn(&goal.text);
        }
    }
    if intent.latest_ask.is_none() && surface.focused {
        intent.latest_ask =
            screen_latest_ask.and_then(crate::mc_data::session_log::summarize_user_turn);
    }

    if intent.overall_goal.is_none() && intent.latest_ask.is_none() {
        None
    } else {
        Some(crate::mc_data::surface_render::SurfaceIntentSummary {
            overall_goal: intent.overall_goal,
            latest_ask: intent.latest_ask,
        })
    }
}

fn read_intent_from_session_path(
    path: &std::path::Path,
) -> Option<crate::mc_data::session_log::ConversationIntent> {
    let text = std::fs::read_to_string(path).ok()?;
    let intent = crate::mc_data::session_log::conversation_intent(&text);
    if intent.overall_goal.is_none() && intent.latest_ask.is_none() {
        None
    } else {
        Some(intent)
    }
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
    use crate::cmux::client::{CmuxClient, Workspace};
    use crate::mc_data::trajectory::TrajectoryDoc;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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

    #[cfg(unix)]
    fn write_fake_cmux(
        path: &std::path::Path,
        workspace_cwd: &std::path::Path,
        tree_succeeds: bool,
    ) {
        let tree_body = if tree_succeeds {
            r#"cat <<'JSON'
{
  "windows": [
    {
      "ref": "window:1",
      "current": true,
      "workspaces": [
        {
          "ref": "workspace:1",
          "panes": [
            {
              "surfaces": [
                {
                  "ref": "surface:1",
                  "pane_ref": "pane:1",
                  "title": "shell",
                  "tty": null,
                  "selected": true,
                  "focused": true,
                  "active": true,
                  "index": 0,
                  "index_in_pane": 0,
                  "type": "terminal"
                }
              ]
            }
          ]
        }
      ]
    }
  ]
}
JSON
"#
            .to_string()
        } else {
            "echo tree failed >&2\nexit 9\n".to_string()
        };
        let script = format!(
            r#"#!/bin/sh
case "$1" in
  list-workspaces)
    cat <<'JSON'
{{
  "window_id": "WIN-1",
  "window_ref": "window:1",
  "workspaces": [
    {{
      "ref": "workspace:1",
      "id": "WS-1",
      "title": "repo",
      "description": null,
      "current_directory": "{}",
      "custom_color": null
    }}
  ]
}}
JSON
    ;;
  tree)
    {}
    ;;
  *)
    echo "unexpected cmux args: $*" >&2
    exit 64
    ;;
esac
"#,
            workspace_cwd.display(),
            tree_body
        );
        std::fs::write(path, script).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    // Trajectory doc with a `## Current surfaces` item that has a surface_id.
    const SAMPLE_WITH_SURFACE: &str = "---
workspace: test-ws
---

## Mission
- Build investment agent

## Current surfaces
- claude · mbp · working              <!-- mc:surface:sid-42 -->

## Beads
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

## Beads
- [ ] sprint-01
";

    fn make_ws(doc_text: &str) -> WorkspaceState {
        let mut doc = TrajectoryDoc::parse(doc_text).unwrap();
        doc.ensure_sections();
        WorkspaceState {
            workspace: Workspace {
                window_id: Some("window-test".to_string()),
                window_ref: Some("window:1".to_string()),
                ref_id: "workspace:3".to_string(),
                uuid: "test-uuid-1".to_string(),
                name: "test-ws".to_string(),
                description: None,
                current_directory: None,
                custom_color: None,
            },
            session: None,
            surfaces: Vec::new(),
            remote_surfaces: HashMap::new(),
            screen_preview: None,
            screen_insights: ScreenInsights::default(),
            tool_call_count: 0,
            notes: None,
            mux_status: None,
            classification: None,
            loading: false,
            summary: None,
            beads: None,
            linear: None,
            linear_open_pending: None,
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

    fn remote_state(
        surface_ref: &str,
        state: &str,
        freshness: crate::mc_data::arcmux_mesh::RemoteFreshness,
    ) -> crate::mc_data::arcmux_mesh::RemoteSurfaceState {
        crate::mc_data::arcmux_mesh::RemoteSurfaceState {
            surface_uuid: "11111111-1111-4111-8111-111111111111".to_string(),
            workspace_uuid: "test-uuid-1".to_string(),
            locator: crate::mc_data::arcmux_mesh::RemoteSessionLocator {
                schema_version: 1,
                device_id: "devbox".to_string(),
                profile_scope: "root".to_string(),
                session_id: format!("s-{surface_ref}"),
                transport_binding_id: None,
            },
            name: Some(format!("remote-{surface_ref}")),
            agent: Some("codex".to_string()),
            state: Some(state.to_string()),
            health: Some("healthy".to_string()),
            launch_cwd: Some("~/Tools/mission-control".to_string()),
            current_work: Some("Implement exact remote surfaces".to_string()),
            freshness,
        }
    }

    fn test_linear_issue(identifier: &str) -> crate::mc_data::linear::LinearIssue {
        crate::mc_data::linear::LinearIssue {
            identifier: identifier.to_string(),
            title: "Open the exact Linear issue".to_string(),
            priority: 2,
            updated_at: Some("2026-07-14T16:00:00Z".to_string()),
            state_name: "In Progress".to_string(),
            state_type: "started".to_string(),
            labels: vec!["group-grader".to_string()],
            url: Some(format!(
                "https://linear.app/reflection-ai/issue/{identifier}/open-the-exact-issue"
            )),
        }
    }

    fn test_linear_view(identifier: &str) -> crate::mc_data::linear::WorkspaceLinearView {
        crate::mc_data::linear::WorkspaceLinearView {
            project_id: "project-1".to_string(),
            required_labels: vec!["group-grader".to_string()],
            feature_name: Some("group-grader".to_string()),
            issues: vec![test_linear_issue(identifier)],
            warning: None,
        }
    }

    fn install_linear_view(app: &mut App, view: crate::mc_data::linear::WorkspaceLinearView) {
        let cursor_item = usize::from(view.feature_name.is_some() && !view.issues.is_empty());
        let items = linear_items_for_view(&view);
        let ws = &mut app.workspaces[0];
        ws.linear = Some(view);
        ws.beads = None;
        ws.trajectory
            .as_mut()
            .unwrap()
            .replace_section_items(crate::mc_data::trajectory::SECTION_GOALS, items);
        ws.edit_state = Some(crate::tui::trajectory_edit::TrajectoryEditState {
            cursor_section: 2,
            cursor_item,
            ..Default::default()
        });
    }

    fn mux_state(
        session_id: &str,
        agent: &str,
        working: bool,
        last_event: &str,
        updated_at: &str,
        last_turn_end_at: Option<&str>,
    ) -> MuxSessionState {
        MuxSessionState {
            session_id: session_id.to_string(),
            agent: agent.to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2026-06-09T13:35:42-07:00").unwrap(),
            updated_at: chrono::DateTime::parse_from_rfc3339(updated_at).unwrap(),
            last_event: last_event.to_string(),
            last_tool: Some("Write".to_string()),
            working,
            turn_count: u64::from(last_turn_end_at.is_some()),
            events_seen: 2,
            last_prompt_submit_at: Some(
                chrono::DateTime::parse_from_rfc3339("2026-06-09T13:36:02-07:00").unwrap(),
            ),
            last_turn_end_at: last_turn_end_at
                .map(|ts| chrono::DateTime::parse_from_rfc3339(ts).unwrap()),
            turn_contract: None,
        }
    }

    fn test_bead_issue(
        id: &str,
        title: &str,
        updated_at: &str,
    ) -> crate::mc_data::beads::BeadIssue {
        crate::mc_data::beads::BeadIssue {
            id: id.to_string(),
            title: title.to_string(),
            status: "open".to_string(),
            priority: Some(2),
            issue_type: Some("task".to_string()),
            assignee: None,
            labels: vec![],
            updated_at: Some(updated_at.to_string()),
        }
    }

    #[test]
    fn task_source_resolution_uses_workspace_then_surface_fallback() {
        use crate::mc_data::project_registry::{ProjectRegistry, TaskSource};

        let temp = tempfile::tempdir().unwrap();
        let registry_path = temp.path().join("projects.yaml");
        std::fs::write(
            &registry_path,
            r#"
projects:
  - project: olympus
    path: ~/Projects/olympus
  - project: agents
    path: ~/agents/blin-agents
platforms:
  - name: olympus
    tracker: linear
    linear:
      project_id: project-1
    features:
      - name: group-grader
        repo: ~/Projects/olympus
        roots: [olympus/projects/minos/graders]
"#,
        )
        .unwrap();
        let registry =
            ProjectRegistry::load_with_home(&registry_path, temp.path()).unwrap();
        let workspace = |uuid: &str, cwd: std::path::PathBuf| Workspace {
            window_id: Some("window-test".to_string()),
            window_ref: Some("window:1".to_string()),
            ref_id: format!("workspace:{uuid}"),
            uuid: uuid.to_string(),
            name: uuid.to_string(),
            description: None,
            current_directory: Some(cwd.to_string_lossy().to_string()),
            custom_color: None,
        };
        let root = temp.path().join("Projects/olympus");
        let feature = root.join("olympus/projects/minos/graders");
        let workspaces = vec![
            workspace("root", root.clone()),
            workspace("fallback", temp.path().join("unregistered-shell")),
        ];
        let mut fallback_roots = HashMap::new();
        fallback_roots.insert("fallback".to_string(), vec![feature]);

        let sources = resolve_task_sources(&workspaces, &fallback_roots, Some(&registry));
        assert!(matches!(
            sources.get("root"),
            Some(TaskSource::Linear(target)) if target.labels.is_empty()
        ));
        assert!(matches!(
            sources.get("fallback"),
            Some(TaskSource::Linear(target))
                if target.labels == ["group-grader".to_string()]
        ));

        let mixed_workspace = Workspace {
            name: "group-graders".to_string(),
            current_directory: Some(
                temp.path()
                    .join("agents/blin-agents")
                    .to_string_lossy()
                    .to_string(),
            ),
            ..workspace("mixed", temp.path().join("agents/blin-agents"))
        };
        let mixed = resolve_task_sources(&[mixed_workspace], &HashMap::new(), Some(&registry));
        assert!(matches!(
            mixed.get("mixed"),
            Some(TaskSource::Linear(target))
                if target.labels == ["group-grader".to_string()]
        ));

        let described_workspace = Workspace {
            name: "evaluation".to_string(),
            description: Some(
                "This workspace builds the group grader feature under Olympus".to_string(),
            ),
            current_directory: Some(
                temp.path()
                    .join("agents/blin-agents")
                    .to_string_lossy()
                    .to_string(),
            ),
            ..workspace("described", temp.path().join("agents/blin-agents"))
        };
        let described =
            resolve_task_sources(&[described_workspace], &HashMap::new(), Some(&registry));
        assert!(matches!(
            described.get("described"),
            Some(TaskSource::Linear(target))
                if target.labels == ["group-grader".to_string()]
        ));
    }

    #[test]
    fn identical_project_and_label_targets_share_one_linear_query_group() {
        use crate::mc_data::project_registry::{LinearTarget, TaskSource};

        let mut sources = HashMap::new();
        for (workspace_id, team_id) in [("one", "team-a"), ("two", "team-b")] {
            sources.insert(
                workspace_id.to_string(),
                TaskSource::Linear(LinearTarget {
                    team_id: Some(team_id.to_string()),
                    project_id: "project-1".to_string(),
                    labels: vec!["group-grader".to_string()],
                    feature_name: Some("group-grader".to_string()),
                }),
            );
        }

        let groups = linear_query_groups(&sources);
        assert_eq!(groups.len(), 1);
        let workspace_ids = groups.values().next().unwrap();
        assert_eq!(workspace_ids.len(), 2);
    }

    #[test]
    fn enter_on_linear_issue_requests_its_exact_desktop_url() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        install_linear_view(&mut app, test_linear_view("MID-508"));

        let actions = app.handle_trajectory_key(key(KeyCode::Enter));

        assert!(actions.is_empty());
        assert_eq!(
            app.take_linear_open_request().as_deref(),
            Some(
                "linear://linear.app/reflection-ai/issue/MID-508/open-the-exact-issue"
            )
        );
        assert!(app.workspaces[0].dispatch_modal.is_none());
    }

    #[test]
    fn linear_non_issue_rows_and_mutation_keys_are_noops() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let unavailable = crate::mc_data::linear::WorkspaceLinearView {
            project_id: "project-1".to_string(),
            required_labels: vec![],
            feature_name: None,
            issues: vec![],
            warning: Some("Linear unavailable: API request failed".to_string()),
        };
        install_linear_view(&mut app, unavailable);
        let before = app.workspaces[0]
            .trajectory
            .as_ref()
            .unwrap()
            .to_markdown();

        app.handle_trajectory_key(key(KeyCode::Enter));
        assert_eq!(app.take_linear_open_request(), None);
        for code in [
            KeyCode::Char(' '),
            KeyCode::Char('x'),
            KeyCode::Char('X'),
            KeyCode::Char('d'),
            KeyCode::Char('o'),
            KeyCode::Char('O'),
            KeyCode::Char('i'),
            KeyCode::Char('J'),
            KeyCode::Char('K'),
        ] {
            assert!(app.handle_trajectory_key(key(code)).is_empty());
        }
        assert_eq!(
            app.workspaces[0]
                .trajectory
                .as_ref()
                .unwrap()
                .to_markdown(),
            before
        );
        assert!(matches!(
            app.workspaces[0].edit_state.as_ref().unwrap().mode,
            crate::tui::trajectory_edit::EditMode::Nav
        ));
    }

    #[test]
    fn transient_linear_failure_retains_only_matching_last_good_target() {
        let previous = test_linear_view("MID-508");
        let mut same_target_failure = crate::mc_data::linear::WorkspaceLinearView {
            project_id: previous.project_id.clone(),
            required_labels: previous.required_labels.clone(),
            feature_name: previous.feature_name.clone(),
            issues: vec![],
            warning: Some("Linear unavailable: API request failed".to_string()),
        };
        retain_last_good_linear(&mut same_target_failure, Some(&previous));
        assert_eq!(same_target_failure.issues[0].identifier, "MID-508");

        let mut other_target_failure = crate::mc_data::linear::WorkspaceLinearView {
            project_id: "project-2".to_string(),
            required_labels: previous.required_labels.clone(),
            feature_name: previous.feature_name.clone(),
            issues: vec![],
            warning: Some("Linear unavailable: API request failed".to_string()),
        };
        retain_last_good_linear(&mut other_target_failure, Some(&previous));
        assert!(other_target_failure.issues.is_empty());
    }

    #[test]
    fn stale_linear_rows_are_removed_without_deleting_local_goals() {
        let projected = linear_items_for_view(&test_linear_view("MID-508"));
        let mut items = projected;
        items.push(crate::mc_data::trajectory::Item {
            text: "Keep this local goal".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        });

        let (cleaned, dropped) = strip_projected_task_rows(&items);

        assert!(dropped);
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].text, "Keep this local goal");
    }

    #[test]
    fn linear_rows_are_segmented_by_registered_feature_name() {
        let items = linear_items_for_view(&test_linear_view("MID-508"));

        assert_eq!(items[0].text, "feature: group-grader");
        assert!(items[1].text.contains("MID-508"));
        assert!(linear_desktop_url_for_row(&test_linear_view("MID-508"), &items[0].text).is_none());
    }

    #[test]
    fn bottom_info_shows_workspace_and_window_in_sidebar_focus() {
        let app = make_app(SAMPLE_WITH_SURFACE);
        assert_eq!(
            app.bottom_info().as_deref(),
            Some("workspace test-uuid-1 · window window-test")
        );
    }

    #[test]
    fn bottom_info_keeps_global_warning_visible() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.global_warning = Some("OpenAI key unavailable".to_string());
        assert_eq!(
            app.bottom_info().as_deref(),
            Some("workspace test-uuid-1 · window window-test · ⚠ OpenAI key unavailable")
        );
    }

    #[test]
    fn remote_status_aggregates_actionable_before_working_before_stale() {
        use crate::mc_data::arcmux_mesh::RemoteFreshness;
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let ws = &mut app.workspaces[0];
        ws.remote_surfaces.insert(
            "surface:working".to_string(),
            remote_state("working", "working", RemoteFreshness::Fresh),
        );
        assert_eq!(ws.agent_state(), AgentState::Working);

        ws.remote_surfaces.insert(
            "surface:waiting".to_string(),
            remote_state("waiting", "waiting", RemoteFreshness::Fresh),
        );
        assert_eq!(ws.agent_state(), AgentState::NeedsMe);

        ws.remote_surfaces.clear();
        ws.remote_surfaces.insert(
            "surface:idle".to_string(),
            remote_state("idle", "idle", RemoteFreshness::Fresh),
        );
        assert_eq!(ws.agent_state(), AgentState::Idle);

        ws.remote_surfaces.clear();
        ws.remote_surfaces.insert(
            "surface:stale".to_string(),
            remote_state("stale", "working", RemoteFreshness::Stale),
        );
        assert_eq!(ws.agent_state(), AgentState::Stale);
    }

    #[test]
    fn mixed_workspace_keeps_local_compact_header_identity() {
        use crate::mc_data::arcmux_mesh::RemoteFreshness;
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let ws = &mut app.workspaces[0];
        ws.surfaces.push(SurfaceInfo {
            title: "claude local".to_string(),
            ref_id: "surface:local".to_string(),
            uuid: Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_string()),
            pane_ref: None,
            tty: None,
            kind: crate::mc_data::surface_kind::SurfaceKind::Claude,
            selected: false,
            focused: false,
            active: false,
            index: None,
            index_in_pane: None,
            surface_type: Some("terminal".to_string()),
        });
        ws.screen_insights.agent = Some("claude".to_string());
        ws.screen_insights.working_dir = Some("~/Projects/local".to_string());
        ws.remote_surfaces.insert(
            "surface:remote".to_string(),
            remote_state("remote", "idle", RemoteFreshness::Fresh),
        );

        assert_eq!(ws.agent_name(), "claude");
        assert_eq!(ws.working_dir(), "~/Projects/local");
        assert_eq!(ws.host_name(), "devbox");
    }

    #[test]
    fn remote_header_selection_is_stable_and_excludes_gone_history() {
        use crate::mc_data::arcmux_mesh::RemoteFreshness;
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let ws = &mut app.workspaces[0];

        let mut first = remote_state("first", "working", RemoteFreshness::Fresh);
        first.agent = Some("claude".to_string());
        first.launch_cwd = Some("~/Projects/first".to_string());
        ws.remote_surfaces.insert("surface:a".to_string(), first);

        let mut second = remote_state("second", "working", RemoteFreshness::Fresh);
        second.agent = Some("codex".to_string());
        second.launch_cwd = Some("~/Projects/second".to_string());
        ws.remote_surfaces.insert("surface:z".to_string(), second);

        let mut gone = remote_state("gone", "waiting", RemoteFreshness::Gone);
        gone.agent = Some("opencode".to_string());
        gone.locator.device_id = "labs".to_string();
        ws.remote_surfaces.insert("surface:0".to_string(), gone);

        assert_eq!(ws.agent_name(), "claude");
        assert_eq!(ws.working_dir(), "~/Projects/first");
        assert_eq!(ws.host_name(), "devbox");
        assert_eq!(ws.agent_state(), AgentState::Working);

        ws.remote_surfaces.get_mut("surface:z").unwrap().state = Some("waiting".to_string());
        assert_eq!(ws.agent_name(), "codex");
        assert_eq!(ws.working_dir(), "~/Projects/second");
        assert_eq!(ws.agent_state(), AgentState::NeedsMe);
    }

    #[test]
    fn gone_remote_is_folded_out_of_current_surfaces() {
        let gone = remote_state(
            "gone",
            "exited",
            crate::mc_data::arcmux_mesh::RemoteFreshness::Gone,
        );
        assert!(!remote_surface_is_current(Some(&gone)));
        assert!(remote_surface_is_current(None));
    }

    #[test]
    fn mesh_fetch_failure_retains_exact_surface_as_stale() {
        let mut old = HashMap::new();
        old.insert(
            "surface:14".to_string(),
            remote_state(
                "surface:14",
                "working",
                crate::mc_data::arcmux_mesh::RemoteFreshness::Fresh,
            ),
        );
        let surfaces = vec![SurfaceInfo {
            title: "anything".to_string(),
            ref_id: "surface:14".to_string(),
            uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
            pane_ref: None,
            tty: None,
            kind: crate::mc_data::surface_kind::SurfaceKind::Remote,
            selected: false,
            focused: false,
            active: false,
            index: None,
            index_in_pane: None,
            surface_type: Some("terminal".to_string()),
        }];

        let retained = retain_remote_surfaces_as_stale(&old, &surfaces);
        let state = retained.get("surface:14").expect("retained binding");
        assert_eq!(
            state.freshness,
            crate::mc_data::arcmux_mesh::RemoteFreshness::Stale
        );
        assert_eq!(state.locator.device_id, "devbox");
    }

    #[test]
    fn bound_remote_peek_uses_exact_cmux_surface_not_local_transcript() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let ws = &mut app.workspaces[0];
        ws.surfaces.push(SurfaceInfo {
            title: "codex-looking remote".to_string(),
            ref_id: "sid-42".to_string(),
            uuid: Some("11111111-1111-4111-8111-111111111111".to_string()),
            pane_ref: None,
            tty: Some("ttys001".to_string()),
            kind: crate::mc_data::surface_kind::SurfaceKind::Codex,
            selected: true,
            focused: true,
            active: true,
            index: Some(0),
            index_in_pane: Some(0),
            surface_type: Some("terminal".to_string()),
        });
        ws.remote_surfaces.insert(
            "sid-42".to_string(),
            remote_state(
                "sid-42",
                "working",
                crate::mc_data::arcmux_mesh::RemoteFreshness::Fresh,
            ),
        );
        let edit = ws.edit_state.get_or_insert_with(Default::default);
        edit.cursor_section = 1;
        edit.cursor_item = 0;

        app.handle_trajectory_key(key(KeyCode::Enter));

        let peek = app.workspaces[0].peek_state.as_ref().expect("peek");
        assert_eq!(peek.surface_ref, "sid-42");
        assert!(matches!(peek.source, crate::tui::peek_view::PeekSource::Shell));
    }

    #[test]
    fn mux_state_drives_agent_state_instead_of_event_name() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.handle_agent_event(&AgentEvent {
            session_id: "s-1".to_string(),
            workspace_id: "test-uuid-1".to_string(),
            event_name: "agent.hook.Stop".to_string(),
        });

        assert_eq!(
            app.workspaces[0].agent_state(),
            AgentState::Idle,
            "hook event name alone must not derive working/waiting"
        );

        app.apply_mux_session_states([mux_state(
            "s-1",
            "grok",
            true,
            "tool_start",
            "2026-06-09T13:36:05-07:00",
            None,
        )]);

        assert_eq!(app.workspaces[0].agent_name(), "grok");
        assert_eq!(app.workspaces[0].agent_state(), AgentState::Working);

        app.apply_mux_session_states([mux_state(
            "s-1",
            "grok",
            false,
            "turn_end",
            "2026-06-09T13:36:08-07:00",
            Some("2026-06-09T13:36:08-07:00"),
        )]);

        assert_eq!(app.workspaces[0].agent_state(), AgentState::NeedsMe);
    }

    #[test]
    fn newest_mux_state_wins_when_workspace_has_multiple_sessions() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        for session_id in ["s-old", "s-new"] {
            app.handle_agent_event(&AgentEvent {
                session_id: session_id.to_string(),
                workspace_id: "test-uuid-1".to_string(),
                event_name: "agent.hook.UserPromptSubmit".to_string(),
            });
        }

        app.apply_mux_session_states([
            mux_state(
                "s-new",
                "claude",
                true,
                "tool_start",
                "2026-06-09T13:36:10-07:00",
                None,
            ),
            mux_state(
                "s-old",
                "grok",
                false,
                "turn_end",
                "2026-06-09T13:36:08-07:00",
                Some("2026-06-09T13:36:08-07:00"),
            ),
        ]);

        assert_eq!(app.workspaces[0].agent_name(), "claude");
        assert_eq!(app.workspaces[0].agent_state(), AgentState::Working);
    }

    #[test]
    fn bottom_info_adds_surface_when_detail_cursor_is_on_surface_row() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.focus = Focus::Detail;
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.cursor_section = 1;
        state.cursor_item = 0;

        assert_eq!(
            app.bottom_info().as_deref(),
            Some("surface sid-42 · workspace test-uuid-1 · window window-test")
        );
    }

    #[tokio::test]
    async fn refresh_projection_replaces_third_section_with_beads() {
        struct HomeGuard(Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe { std::env::set_var("HOME", v) },
                    None => unsafe { std::env::remove_var("HOME") },
                }
            }
        }

        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let _guard = HomeGuard(std::env::var_os("HOME"));
        unsafe {
            std::env::set_var("HOME", home.path());
        }

        let issue = crate::mc_data::beads::BeadIssue {
            id: "repo-7".to_string(),
            title: "Show Beads in mission control".to_string(),
            status: "in_progress".to_string(),
            priority: Some(1),
            issue_type: Some("feature".to_string()),
            assignee: Some("blin".to_string()),
            labels: vec!["tui".to_string()],
            updated_at: None,
        };
        let mut beads_by_ws_id = HashMap::new();
        beads_by_ws_id.insert(
            "uuid-beads".to_string(),
            crate::mc_data::beads::WorkspaceBeadsView {
                repos: vec![crate::mc_data::beads::BeadsView {
                    repo_path: repo.path().to_path_buf(),
                    source: crate::mc_data::beads::BeadsSource::BdList,
                    issues: vec![issue],
                }],
                repo_by_surface_ref: HashMap::new(),
            },
        );
        let snap = RefreshSnapshot {
            workspaces: vec![Workspace {
                window_id: Some("window-test".to_string()),
                window_ref: Some("window:1".to_string()),
                ref_id: "workspace:9".to_string(),
                uuid: "uuid-beads".to_string(),
                name: "repo".to_string(),
                description: None,
                current_directory: Some(repo.path().to_string_lossy().to_string()),
                custom_color: None,
            }],
            surfaces_map: HashMap::new(),
            sessions_by_ws_id: HashMap::new(),
            beads_by_ws_id,
            linear_by_ws_id: HashMap::new(),
            surface_intents_by_ws_id: HashMap::new(),
            remote_mesh: None,
            mesh_warning: None,
        };
        let mut app = App::new();
        app.apply_refresh_snapshot(snap, None).await;
        let doc = app.workspaces[0].trajectory.as_ref().expect("trajectory");
        let beads = doc
            .section(crate::mc_data::trajectory::SECTION_GOALS)
            .expect("Beads section");
        assert_eq!(beads.name, "Beads");
        // [0] = always-present repo header, [1] = the issue.
        assert_eq!(beads.items.len(), 2);
        assert!(beads.items[0].text.starts_with("repo: "));
        assert!(beads.items[1].text.contains("repo-7 in-progress"));
        assert!(
            beads.items[1]
                .text
                .contains("Show Beads in mission control")
        );
        assert_eq!(beads.items[1].checked, Some(false));
    }

    #[tokio::test]
    async fn refresh_projection_persists_stable_mission_preview_and_cancel() {
        struct HomeGuard(Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("HOME", value) },
                    None => unsafe { std::env::remove_var("HOME") },
                }
            }
        }

        let home = tempfile::tempdir().unwrap();
        let _guard = HomeGuard(std::env::var_os("HOME"));
        unsafe {
            std::env::set_var("HOME", home.path());
        }

        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.handle_trajectory_key(key(KeyCode::Char('x')));
        let workspace = app.workspaces[0].workspace.clone();
        let snapshot = |surface_title: &str| {
            let mut surfaces_map = HashMap::new();
            surfaces_map.insert(
                workspace.ref_id.clone(),
                vec![SurfaceInfo {
                    title: surface_title.to_string(),
                    ref_id: "surface:stable-preview".to_string(),
                    uuid: None,
                    pane_ref: None,
                    tty: None,
                    kind: crate::mc_data::surface_kind::SurfaceKind::Shell,
                    selected: true,
                    focused: true,
                    active: true,
                    index: Some(0),
                    index_in_pane: Some(0),
                    surface_type: None,
                }],
            );
            RefreshSnapshot {
                workspaces: vec![workspace.clone()],
                surfaces_map,
                sessions_by_ws_id: HashMap::new(),
                beads_by_ws_id: HashMap::new(),
                linear_by_ws_id: HashMap::new(),
                surface_intents_by_ws_id: HashMap::new(),
                remote_mesh: None,
                mesh_warning: None,
            }
        };

        let first = snapshot("first projected surface");
        app.apply_refresh_snapshot(first, None).await;
        assert_eq!(
            app.workspaces[0]
                .trajectory
                .as_ref()
                .unwrap()
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .unwrap()
                .items[0]
                .checked,
            Some(true)
        );
        let persisted = crate::mc_data::trajectory::TrajectoryDoc::load_from_file(
            &crate::mc_data::paths::trajectory_path("test-uuid-1"),
        )
        .unwrap();
        assert_eq!(
            persisted
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .unwrap()
                .items[0]
                .checked,
            Some(false)
        );
        assert!(persisted.mission_history.is_empty());

        app.handle_trajectory_key(key(KeyCode::Char('x')));
        let second = snapshot("second projected surface");
        app.apply_refresh_snapshot(second, None).await;
        let persisted = crate::mc_data::trajectory::TrajectoryDoc::load_from_file(
            &crate::mc_data::paths::trajectory_path("test-uuid-1"),
        )
        .unwrap();
        assert_eq!(
            persisted
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .unwrap()
                .items[0]
                .checked,
            Some(false)
        );
        assert!(persisted.mission_history.is_empty());
        assert!(
            persisted
                .section(crate::mc_data::trajectory::SECTION_CURRENT_SURFACES)
                .unwrap()
                .items[0]
                .text
                .contains("second projected surface")
        );
    }

    #[test]
    fn beads_refresh_targets_collect_visible_beads_repos() {
        let repo_a = std::path::PathBuf::from("/tmp/repo-a");
        let repo_b = std::path::PathBuf::from("/tmp/repo-b");
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let mut repo_by_surface_ref = HashMap::new();
        repo_by_surface_ref.insert("sid-42".to_string(), repo_b.clone());
        app.workspaces[0].beads = Some(crate::mc_data::beads::WorkspaceBeadsView {
            repos: vec![
                crate::mc_data::beads::BeadsView {
                    repo_path: repo_a.clone(),
                    source: crate::mc_data::beads::BeadsSource::BdList,
                    issues: vec![],
                },
                crate::mc_data::beads::BeadsView {
                    repo_path: repo_b.clone(),
                    source: crate::mc_data::beads::BeadsSource::BdList,
                    issues: vec![],
                },
            ],
            repo_by_surface_ref: repo_by_surface_ref.clone(),
        });

        let targets = app.beads_refresh_targets();

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].generation, app.beads_generation);
        assert_eq!(targets[0].workspace_id, "test-uuid-1");
        assert_eq!(targets[0].repo_roots, vec![repo_a, repo_b]);
        assert_eq!(targets[0].repo_by_surface_ref, repo_by_surface_ref);
    }

    #[tokio::test]
    async fn beads_refresh_snapshot_updates_beads_section_without_full_refresh() {
        let repo = std::path::PathBuf::from("/tmp/repo-live");
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].beads = Some(crate::mc_data::beads::WorkspaceBeadsView {
            repos: vec![crate::mc_data::beads::BeadsView {
                repo_path: repo.clone(),
                source: crate::mc_data::beads::BeadsSource::BdList,
                issues: vec![test_bead_issue(
                    "old-1",
                    "Old issue",
                    "2026-06-04T18:00:00Z",
                )],
            }],
            repo_by_surface_ref: HashMap::new(),
        });

        let mut beads_by_ws_id = HashMap::new();
        beads_by_ws_id.insert(
            "test-uuid-1".to_string(),
            crate::mc_data::beads::WorkspaceBeadsView {
                repos: vec![crate::mc_data::beads::BeadsView {
                    repo_path: repo,
                    source: crate::mc_data::beads::BeadsSource::BdList,
                    issues: vec![test_bead_issue(
                        "new-1",
                        "Newly created issue",
                        "2026-06-04T19:00:00Z",
                    )],
                }],
                repo_by_surface_ref: HashMap::new(),
            },
        );

        app.apply_beads_refresh_snapshot_with_saver(
            BeadsRefreshSnapshot {
                generation: app.beads_generation,
                beads_by_ws_id,
            },
            |_, _| {},
        );

        let beads = app.workspaces[0]
            .trajectory
            .as_ref()
            .unwrap()
            .section(crate::mc_data::trajectory::SECTION_GOALS)
            .unwrap();
        // [0] = always-present repo header, [1] = the issue.
        assert_eq!(beads.items.len(), 2);
        assert!(beads.items[0].text.starts_with("repo: "));
        assert!(beads.items[1].text.contains("new-1"));
        assert!(beads.items[1].text.contains("Newly created issue"));
        assert!(beads.items.iter().all(|i| !i.text.contains("old-1")));
    }

    #[test]
    fn beads_refresh_persists_stable_mission_during_preview_and_after_cancel() {
        let repo = std::path::PathBuf::from("/tmp/repo-live");
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.handle_trajectory_key(key(KeyCode::Char('x')));

        let snapshot = |generation, id: &str| {
            let mut beads_by_ws_id = HashMap::new();
            beads_by_ws_id.insert(
                "test-uuid-1".to_string(),
                crate::mc_data::beads::WorkspaceBeadsView {
                    repos: vec![crate::mc_data::beads::BeadsView {
                        repo_path: repo.clone(),
                        source: crate::mc_data::beads::BeadsSource::BdList,
                        issues: vec![test_bead_issue(id, "Fresh issue", "2026-06-04T19:00:00Z")],
                    }],
                    repo_by_surface_ref: HashMap::new(),
                },
            );
            BeadsRefreshSnapshot {
                generation,
                beads_by_ws_id,
            }
        };

        let mut saved_preview = None;
        app.apply_beads_refresh_snapshot_with_saver(
            snapshot(app.beads_generation, "new-1"),
            |_, doc| saved_preview = Some(doc.clone()),
        );

        let live_mission = app.workspaces[0]
            .trajectory
            .as_ref()
            .unwrap()
            .section(crate::mc_data::trajectory::SECTION_MISSION)
            .unwrap();
        assert_eq!(live_mission.items[0].checked, Some(true));
        let restarted = crate::mc_data::trajectory::TrajectoryDoc::parse(
            &saved_preview.unwrap().to_markdown(),
        )
        .unwrap();
        assert_eq!(
            restarted
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .unwrap()
                .items[0]
                .checked,
            Some(false)
        );
        assert!(restarted.mission_history.is_empty());

        // Cancelling within the grace period leaves the same stable restart
        // state, and a subsequent real Beads write can persist normally.
        app.handle_trajectory_key(key(KeyCode::Char('x')));
        assert!(!app.workspaces[0]
            .edit_state
            .as_ref()
            .unwrap()
            .has_pending_mission_moves());
        let mut saved_after_cancel = None;
        app.apply_beads_refresh_snapshot_with_saver(
            snapshot(app.beads_generation, "new-2"),
            |_, doc| saved_after_cancel = Some(doc.clone()),
        );
        let restarted = crate::mc_data::trajectory::TrajectoryDoc::parse(
            &saved_after_cancel.unwrap().to_markdown(),
        )
        .unwrap();
        assert_eq!(
            restarted
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .unwrap()
                .items[0]
                .checked,
            Some(false)
        );
        assert!(restarted.mission_history.is_empty());
    }

    #[tokio::test]
    async fn beads_refresh_snapshot_ignores_stale_generation() {
        let repo = std::path::PathBuf::from("/tmp/repo-live");
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].beads = Some(crate::mc_data::beads::WorkspaceBeadsView {
            repos: vec![crate::mc_data::beads::BeadsView {
                repo_path: repo.clone(),
                source: crate::mc_data::beads::BeadsSource::BdList,
                issues: vec![test_bead_issue(
                    "old-1",
                    "Old issue",
                    "2026-06-04T18:00:00Z",
                )],
            }],
            repo_by_surface_ref: HashMap::new(),
        });
        let stale_generation = app.beads_generation;
        app.beads_generation = app.beads_generation.wrapping_add(1);

        let mut beads_by_ws_id = HashMap::new();
        beads_by_ws_id.insert(
            "test-uuid-1".to_string(),
            crate::mc_data::beads::WorkspaceBeadsView {
                repos: vec![crate::mc_data::beads::BeadsView {
                    repo_path: repo,
                    source: crate::mc_data::beads::BeadsSource::BdList,
                    issues: vec![test_bead_issue(
                        "new-1",
                        "Stale issue should not apply",
                        "2026-06-04T19:00:00Z",
                    )],
                }],
                repo_by_surface_ref: HashMap::new(),
            },
        );

        app.apply_beads_refresh_snapshot(BeadsRefreshSnapshot {
            generation: stale_generation,
            beads_by_ws_id,
        })
        .await;

        let beads = app.workspaces[0]
            .trajectory
            .as_ref()
            .unwrap()
            .section(crate::mc_data::trajectory::SECTION_GOALS)
            .unwrap();
        assert_eq!(beads.items[0].text, "sprint-01");
        let active_beads = app.workspaces[0].beads.as_ref().unwrap();
        assert_eq!(active_beads.repos[0].issues[0].id, "old-1");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refresh_skips_registry_write_when_surface_snapshot_is_incomplete() {
        struct HomeGuard(Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe { std::env::set_var("HOME", v) },
                    None => unsafe { std::env::remove_var("HOME") },
                }
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let histories = tmp.path().join("histories");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&histories).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let _guard = HomeGuard(std::env::var_os("HOME"));
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let registry_dir = home.join("data/mission-control/windows/WIN-1");
        std::fs::create_dir_all(registry_dir.join("surfaces")).unwrap();
        std::fs::write(registry_dir.join("window.json"), "\"sentinel\"").unwrap();
        std::fs::write(registry_dir.join("surfaces/surface_1.json"), "\"keep\"").unwrap();

        let fake_cmux = tmp.path().join("cmux");
        write_fake_cmux(&fake_cmux, &repo, false);
        let client = CmuxClient::new(
            fake_cmux.display().to_string(),
            tmp.path().join("cmux.sock"),
        );

        gather_refresh_snapshot(&client, &histories).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(registry_dir.join("window.json")).unwrap(),
            "\"sentinel\""
        );
        assert!(
            registry_dir.join("surfaces/surface_1.json").exists(),
            "stale but last-known-good surface record should not be pruned by a failed refresh"
        );
    }

    #[test]
    fn beads_empty_state_reports_unavailable_source() {
        let view = crate::mc_data::beads::WorkspaceBeadsView {
            repos: vec![crate::mc_data::beads::BeadsView {
                repo_path: std::path::PathBuf::from("/tmp/repo"),
                source: crate::mc_data::beads::BeadsSource::Unavailable,
                issues: vec![],
            }],
            repo_by_surface_ref: HashMap::new(),
        };
        let items = beads_items_for_view(&view, None);
        // [0] = always-present repo header, [1] = the unavailable note.
        assert_eq!(items.len(), 2);
        assert!(items[0].text.starts_with("repo: "));
        assert!(items[1].text.contains("beads unavailable"));
    }

    #[test]
    fn beads_projection_prioritizes_highlighted_surface_repo() {
        const SAMPLE_MULTI_REPO_SURFACES: &str = "---
workspace: test-ws
---

## Mission
- Build investment agent

## Current surfaces
- claude · repo-a              <!-- mc:surface:sid-a -->
- codex · repo-b              <!-- mc:surface:sid-b -->

## Beads
- [ ] old
";
        let repo_a = std::path::PathBuf::from("/tmp/repo-a");
        let repo_b = std::path::PathBuf::from("/tmp/repo-b");
        let issue_a = crate::mc_data::beads::BeadIssue {
            id: "A-1".to_string(),
            title: "Repo A task".to_string(),
            status: "open".to_string(),
            priority: Some(2),
            issue_type: None,
            assignee: None,
            labels: vec![],
            updated_at: None,
        };
        let issue_b = crate::mc_data::beads::BeadIssue {
            id: "B-1".to_string(),
            title: "Repo B task".to_string(),
            status: "in_progress".to_string(),
            priority: Some(1),
            issue_type: None,
            assignee: None,
            labels: vec![],
            updated_at: None,
        };
        let mut repo_by_surface_ref = HashMap::new();
        repo_by_surface_ref.insert("sid-a".to_string(), repo_a.clone());
        repo_by_surface_ref.insert("sid-b".to_string(), repo_b.clone());

        let mut app = make_app(SAMPLE_MULTI_REPO_SURFACES);
        app.workspaces[0].beads = Some(crate::mc_data::beads::WorkspaceBeadsView {
            repos: vec![
                crate::mc_data::beads::BeadsView {
                    repo_path: repo_a,
                    source: crate::mc_data::beads::BeadsSource::BdList,
                    issues: vec![issue_a],
                },
                crate::mc_data::beads::BeadsView {
                    repo_path: repo_b,
                    source: crate::mc_data::beads::BeadsSource::BdList,
                    issues: vec![issue_b],
                },
            ],
            repo_by_surface_ref,
        });
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.cursor_section = 1;
        state.cursor_item = 1;

        let highlighted = highlighted_surface_repo(&app.workspaces[0]);
        let items = beads_items_for_view(
            app.workspaces[0].beads.as_ref().unwrap(),
            highlighted.as_deref(),
        );

        assert_eq!(items[0].text, "repo: repo-b");
        assert!(items[1].text.contains("B-1 in-progress"));
        assert_eq!(items[2].text, "repo: repo-a");
        assert!(items[3].text.contains("A-1 open"));
    }

    #[test]
    fn live_beads_rows_are_read_only_in_detail_editor() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].beads = Some(crate::mc_data::beads::WorkspaceBeadsView {
            repos: vec![crate::mc_data::beads::BeadsView {
                repo_path: std::path::PathBuf::from("/tmp/repo"),
                source: crate::mc_data::beads::BeadsSource::BdList,
                issues: vec![],
            }],
            repo_by_surface_ref: HashMap::new(),
        });
        let state = app.workspaces[0]
            .edit_state
            .get_or_insert_with(Default::default);
        state.cursor_section = 2;
        state.cursor_item = 0;

        let before = app.workspaces[0].trajectory.as_ref().unwrap().to_markdown();
        app.handle_trajectory_key(key(KeyCode::Enter));
        app.handle_trajectory_key(key(KeyCode::Char(' ')));
        app.handle_trajectory_key(key(KeyCode::Char('x')));
        app.handle_trajectory_key(key(KeyCode::Char('i')));

        let after = app.workspaces[0].trajectory.as_ref().unwrap().to_markdown();
        assert_eq!(after, before);
        assert!(app.workspaces[0].dispatch_modal.is_none());
        assert!(matches!(
            app.workspaces[0].edit_state.as_ref().unwrap().mode,
            crate::tui::trajectory_edit::EditMode::Nav
        ));
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
            "workspace:7".to_string(), // workspace_ref — what select-workspace needs
            "surface:42".to_string(),  // surface_ref — for future per-surface use
            "test".to_string(),
            crate::tui::peek_view::PeekSource::Shell,
        ));
        app.workspaces[0].peek_yield_pending = true;

        let yielded = app.take_peek_yield();

        assert_eq!(
            yielded,
            Some(("workspace:7".to_string(), "surface:42".to_string())),
            "yield returns (workspace_ref, surface_ref): select the workspace, then focus the surface's pane"
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
    fn workspaces_due_for_regen_excludes_pending_mission_relocation() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].regen.events_since_last_regen = 15;
        app.handle_trajectory_key(key(KeyCode::Char('x')));

        assert!(
            app.workspaces[0]
                .edit_state
                .as_ref()
                .unwrap()
                .has_pending_mission_moves()
        );
        assert!(
            app.workspaces_due_for_regen().is_empty(),
            "regen must not normalize a checked Mission preview before its deadline"
        );
    }

    #[test]
    fn failed_settle_save_keeps_preview_and_retries_the_due_move() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.handle_trajectory_key(key(KeyCode::Char('x')));
        let due_at = Instant::now() + std::time::Duration::from_secs(6);

        let settled = app.settle_pending_mission_moves_with_saver(
            due_at,
            |_, _, _, _| anyhow::bail!("deterministic save failure"),
        );
        assert!(settled.is_empty());
        let live_doc = app.workspaces[0].trajectory.as_ref().unwrap();
        assert_eq!(
            live_doc
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .unwrap()
                .items[0]
                .checked,
            Some(true)
        );
        assert!(live_doc.mission_history.is_empty());
        assert!(app.workspaces[0]
            .edit_state
            .as_ref()
            .unwrap()
            .has_pending_mission_moves());

        let mut saved_actions = 0;
        let settled = app.settle_pending_mission_moves_with_saver(
            due_at,
            |_, _, _, actions| {
                saved_actions = actions.len();
                Ok(())
            },
        );
        assert_eq!(saved_actions, 1);
        assert_eq!(settled.len(), 1);
        let live_doc = app.workspaces[0].trajectory.as_ref().unwrap();
        assert!(
            live_doc
                .section(crate::mc_data::trajectory::SECTION_MISSION)
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(live_doc.mission_history.len(), 1);
        assert!(!app.workspaces[0]
            .edit_state
            .as_ref()
            .unwrap()
            .has_pending_mission_moves());
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
    fn workspaces_due_for_regen_includes_empty_mission_without_events() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0]
            .trajectory
            .as_mut()
            .unwrap()
            .replace_section_items(crate::mc_data::trajectory::SECTION_MISSION, Vec::new());
        app.workspaces[0].regen.events_since_last_regen = 0;
        app.workspaces[0].regen.last_regen_at = None;

        assert_eq!(
            app.workspaces_due_for_regen(),
            vec!["test-uuid-1".to_string()],
            "an empty Mission should self-heal without waiting for edit events"
        );
    }

    #[test]
    fn completed_mission_history_does_not_self_heal_into_active_work() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        let doc = app.workspaces[0].trajectory.as_mut().unwrap();
        let mut completed = doc
            .section(crate::mc_data::trajectory::SECTION_MISSION)
            .unwrap()
            .items[0]
            .clone();
        completed.checked = Some(true);
        doc.replace_section_items(crate::mc_data::trajectory::SECTION_MISSION, Vec::new());
        doc.mission_history.push(completed);
        app.workspaces[0].regen.events_since_last_regen = 0;
        app.workspaces[0].regen.last_regen_at = None;

        assert!(app.workspaces_due_for_regen().is_empty());
    }

    #[test]
    fn apply_regenerated_trajectory_replaces_and_resets() {
        use crate::mc_data::trajectory::TrajectoryDoc;

        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].regen.events_since_last_regen = 5;
        app.workspaces[0].regen.regen_in_flight = true;

        let new_doc_text = "---\nworkspace: test-ws\n---\n\n## Mission\n- Updated goal\n\n## Current surfaces\n\n## Beads\n- [ ] new task\n";
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
    fn regen_preserves_registry_projected_linear_rows() {
        let mut app = make_app(SAMPLE_WITH_SURFACE);
        install_linear_view(&mut app, test_linear_view("MID-508"));
        let generated = TrajectoryDoc::parse(
            "---\nworkspace: test-ws\n---\n\n## Mission\n- Updated goal\n\n## Current surfaces\n\n## Beads\n- [ ] model-owned task\n",
        )
        .unwrap();

        app.apply_regenerated_trajectory("test-uuid-1", generated);

        let tasks = app.workspaces[0]
            .trajectory
            .as_ref()
            .unwrap()
            .section(crate::mc_data::trajectory::SECTION_GOALS)
            .unwrap();
        assert_eq!(tasks.items.len(), 2);
        assert_eq!(tasks.items[0].text, "feature: group-grader");
        assert!(tasks.items[1].text.contains("MID-508"));
        assert!(
            tasks
                .items
                .iter()
                .all(|item| !item.text.contains("model-owned"))
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

        let new_doc_text = "---\nworkspace: test-ws\n---\n\n## Mission\n- Should not appear\n\n## Current surfaces\n\n## Beads\n";
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

    #[test]
    fn apply_regenerated_trajectory_skips_pending_mission_relocation() {
        use crate::mc_data::trajectory::TrajectoryDoc;

        let mut app = make_app(SAMPLE_WITH_SURFACE);
        app.workspaces[0].regen.regen_in_flight = true;
        app.handle_trajectory_key(key(KeyCode::Char('x')));

        let new_doc = TrajectoryDoc::parse(
            "---\nworkspace: test-ws\n---\n\n## Mission\n- Should not appear\n\n## Current surfaces\n\n## Beads\n",
        )
        .unwrap();
        app.apply_regenerated_trajectory("test-uuid-1", new_doc);

        let mission = app.workspaces[0]
            .trajectory
            .as_ref()
            .unwrap()
            .section(crate::mc_data::trajectory::SECTION_MISSION)
            .unwrap();
        assert_eq!(mission.items[0].text, "Build investment agent");
        assert_eq!(mission.items[0].checked, Some(true));
        assert!(
            app.workspaces[0]
                .edit_state
                .as_ref()
                .unwrap()
                .has_pending_mission_moves()
        );
        assert!(!app.workspaces[0].regen.regen_in_flight);
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

    #[test]
    fn projected_task_row_distinguishes_injected_rows_from_goals() {
        // mc-injected bead rows + headers.
        assert!(is_projected_task_row("[P0] GTR-1 open · elonco send delivers brief"));
        assert!(is_projected_task_row("[P?] foo-1 in-progress · bar"));
        assert!(is_projected_task_row("repo: elonco"));
        assert!(is_projected_task_row("No active beads in gmail-triage"));
        assert!(is_projected_task_row("Beads unavailable in foo (bd list failed)"));
        assert!(is_projected_task_row("No active Linear issues"));
        assert!(is_projected_task_row("(Linear unavailable: API request failed)"));
        assert!(is_projected_task_row("(stale — Linear refresh unavailable)"));
        // Local/legacy goal rows must NOT be treated as beads.
        assert!(!is_projected_task_row("[MSC-1] build the thing"));
        assert!(!is_projected_task_row("ship the feature"));
        assert!(!is_projected_task_row("[Plan] outline the work"));
        assert!(!is_projected_task_row(""));
    }

    #[test]
    fn intent_not_broadcast_to_nonfocused_surfaces() {
        use crate::cmux::client::SurfaceInfo;
        use crate::mc_data::surface_kind::SurfaceKind;
        let mk = |focused: bool| SurfaceInfo {
            title: "Claude Code".to_string(),
            ref_id: "surface:99".to_string(),
            uuid: None,
            pane_ref: None,
            tty: None,
            kind: SurfaceKind::Claude,
            selected: false,
            focused,
            active: false,
            index: None,
            index_in_pane: None,
            surface_type: None,
        };
        let goals = crate::mc_data::goals_json::GoalsFile::default();
        // A non-focused agent surface with no resolved intent must NOT borrow
        // the workspace-level on-screen prompt (that's another surface's).
        let nf = surface_intent_summary(
            None,
            None,
            Some("update arcmux to support native LLMs"),
            &mk(false),
            SurfaceKind::Claude,
            &goals,
        );
        assert!(
            nf.is_none(),
            "non-focused surface borrowed a prompt: {nf:?}"
        );
        // The focused surface does pick it up.
        let f = surface_intent_summary(
            None,
            None,
            Some("update arcmux to support native LLMs"),
            &mk(true),
            SurfaceKind::Claude,
            &goals,
        );
        assert!(
            f.and_then(|i| i.latest_ask).is_some(),
            "focused surface should adopt the on-screen prompt"
        );
    }
}
