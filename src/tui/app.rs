use crate::cmux::client::{CmuxClient, SurfaceInfo, Workspace};
use crate::cmux::events::AgentEvent;
use crate::llm::Summary;
use crate::llm::typesafe::{ScreenClassification, TypeSafeClassifier};
use crate::session::file::{self, SessionFile};
use crate::session::watcher::FileChanged;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

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
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    slug.trim_matches('-').to_string()
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
        let is_pure_divider =
            !trimmed.is_empty() && trimmed.chars().all(|c| c == '─');
        if is_pure_divider {
            input_area_start = i;
            found_divider = true;
        } else if found_divider {
            // Inside the input area block - skip prompts, status bars, empty lines
            if trimmed.starts_with('⏵')
                || trimmed.starts_with('❯')
                || trimmed.is_empty()
            {
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
            && !matches!(first_char, '─' | '│' | '⏵' | '└' | '├' | '⎿' | '⏺' | '●'
                         | '▸' | '▹' | '►' | '▶' | '›' | '❯');
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
            && !matches!(first_char, '─' | '│' | '⏵' | '└' | '├' | '⎿' | '⏺' | '●'
                         | '▸' | '▹' | '►' | '▶' | '›' | '❯');

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
            else if trimmed.contains(" for ") && trimmed.contains('s') && !trimmed.contains('…') {
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
    if full_lower.contains(":claude") || full_lower.contains("\"claude") || full_lower.contains("claude code") {
        insights.agent = Some("claude".to_string());
    } else if full_lower.contains(":codex") || full_lower.contains("\"codex") || full_lower.contains("gpt-") {
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
        let end = after.find(|c: char| c == '─' || c == ')' || c == '·').unwrap_or(after.len());
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
            if t.contains("claude") { return "claude"; }
            if t.contains("codex") { return "codex"; }
            if t.contains("opencode") { return "opencode"; }
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
        }
    }

    pub async fn refresh_workspaces(
        &mut self,
        client: &CmuxClient,
        histories_dir: &std::path::Path,
    ) -> Result<()> {
        let workspaces = client.list_workspaces().await?;
        let surfaces_map = client.get_surfaces().await.unwrap_or_default();
        let session_files = file::list_session_files(histories_dir).unwrap_or_default();

        // Parse all sessions, index by workspace_id
        let mut sessions_by_ws_id: HashMap<String, SessionFile> = HashMap::new();
        for path in &session_files {
            if let Ok(sf) = SessionFile::parse(path) {
                if let Some(ref ws_id) = sf.frontmatter.workspace_id {
                    sessions_by_ws_id.entry(ws_id.clone()).or_insert(sf);
                }
            }
        }

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

        self.workspaces = workspaces
            .into_iter()
            .map(|ws| {
                let session = sessions_by_ws_id.remove(&ws.uuid);
                let surfaces = surfaces_map.get(&ws.ref_id).cloned().unwrap_or_default();
                let tool_call_count = old_counts.get(&ws.uuid).copied().unwrap_or(0);
                let screen_preview = old_previews.get(&ws.uuid).cloned().flatten();
                // Reuse existing insights (parsed from full 100-line capture)
                // rather than re-parsing from the truncated 15-line preview
                let screen_insights = old_insights.get(&ws.uuid).cloned()
                    .unwrap_or_default();
                // Provision the per-workspace data dir + display symlink.
                // Non-fatal: log to stderr and continue so mc-tui never
                // crashes just because the home dir is unwriteable.
                if let Err(e) = crate::mc_data::workspace::ensure_workspace(
                    &ws.uuid,
                    &ws.name,
                    &ws.name, // project defaults to name for now
                ) {
                    eprintln!("ensure_workspace({}): {e:?}", &ws.uuid);
                }
                // Load the trajectory doc only when the file actually exists.
                // load_from_file synthesises a default doc on NotFound, which
                // would wrongly show an empty trajectory panel; the .exists()
                // guard keeps None as the "no trajectory yet" signal that
                // detail.rs uses to fall back to the legacy rendering.
                let trajectory = {
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

        Ok(())
    }

    pub fn handle_agent_event(&mut self, event: &AgentEvent) {
        self.session_to_workspace
            .insert(event.session_id.clone(), event.workspace_id.clone());

        if let Some(&idx) = self.workspace_index.get(&event.workspace_id) {
            self.workspaces[idx].tool_call_count += 1;
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

        if bullets_changed {
            Some(ws_id)
        } else {
            None
        }
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
            spawn_screen_task(ws.workspace.uuid.clone(), ws.workspace.ref_id.clone(), client, classifier, tx);
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
        use tokio::time::{timeout, Duration};
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
