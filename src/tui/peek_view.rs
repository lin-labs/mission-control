/// Peek mode: read-only screen view for a surface.
///
/// Activated by pressing Enter on a `## Current surfaces` row in nav mode.
/// Shows a live (polled) read of the surface's screen content.
/// Keys: j/k scroll · g/G top/bottom · Esc back · Enter yield (select workspace).
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::path::{Path, PathBuf};
use std::time::Instant;

// ──────────────────────────────────────────────────────────────────────────────
// State
// ──────────────────────────────────────────────────────────────────────────────

/// Maximum number of lines to keep in the rolling screen buffer.
pub const BUFFER_MAX_LINES: usize = 500;

/// Interval between screen polls while in peek mode.
pub const PEEK_POLL_INTERVAL_SECS: u64 = 1;

/// Distinguishes between an agent surface (session-log based) and a shell
/// surface (live cmux read-screen based).
#[derive(Debug, Clone)]
pub enum PeekSource {
    /// Agent surface — buffer is rendered from the workspace's session log.
    /// On each poll tick we re-read the file, parse turns, and format them
    /// with the truncation rule applied to non-user turns.
    Agent { session_path: PathBuf },
    /// Generic terminal — buffer is rendered from cmux read-screen polling.
    Shell,
}

/// State for peek mode (reading a surface's screen).
#[derive(Debug, Clone)]
pub struct PeekState {
    /// The workspace ref (e.g., "workspace:3") used to call `cmux read-screen`.
    /// `read-screen` rejects surface refs — passing one yields "Workspace not
    /// found" — so this must be the workspace ref, not the surface ref.
    pub workspace_ref: String,
    /// The surface ref (e.g., "surface:121") this peek targets. Carried for
    /// future use (e.g., when cmux adds a per-surface read-screen) and for
    /// debugging; not currently passed to read-screen.
    pub surface_ref: String,
    /// Human-readable label shown in the peek title bar.
    pub surface_label: String,
    /// Whether this peek reads a session log (Agent) or live cmux screen (Shell).
    pub source: PeekSource,
    /// How many lines from the top of the buffer to display.
    pub scroll_offset: u16,
    /// Rolling buffer of recent screen content (~500 lines max).
    pub screen_buffer: Vec<String>,
    /// When we last polled the screen.
    pub last_poll: Option<Instant>,
    /// Whether we're currently waiting for a poll to complete.
    pub polling: bool,
    /// When true, every `ingest_screen` snaps `scroll_offset` to the bottom
    /// so the user sees the most recent content by default. Any manual
    /// scroll up (j/k/page/g) disables auto-follow; `G` (go_bottom) or `f`
    /// re-enables it.
    pub auto_follow: bool,
}

/// Pagination jump for Space (page-down) / `-` (page-up). Fixed-size rather
/// than tied to visible area height because PeekState doesn't know the
/// render area's height.
pub const PAGE_SIZE: u16 = 10;

impl PeekState {
    pub fn new(
        workspace_ref: String,
        surface_ref: String,
        surface_label: String,
        source: PeekSource,
    ) -> Self {
        Self {
            workspace_ref,
            surface_ref,
            surface_label,
            source,
            scroll_offset: 0,
            screen_buffer: Vec::new(),
            last_poll: None,
            polling: false,
            auto_follow: true,
        }
    }

    /// Whether this peek source actually needs the cmux read-screen call.
    /// Agent surfaces don't — they read the session log directly.
    pub fn uses_cmux_screen(&self) -> bool {
        matches!(self.source, PeekSource::Shell)
    }

    /// Check whether enough time has passed to trigger another poll.
    pub fn should_poll(&self) -> bool {
        if self.polling {
            return false;
        }
        match self.last_poll {
            None => true,
            Some(t) => t.elapsed().as_secs() >= PEEK_POLL_INTERVAL_SECS,
        }
    }

    /// Ingest new screen content into the rolling buffer.
    pub fn ingest_screen(&mut self, text: &str) {
        for line in text.lines() {
            self.screen_buffer.push(line.to_string());
        }
        // Keep only the most recent BUFFER_MAX_LINES lines.
        if self.screen_buffer.len() > BUFFER_MAX_LINES {
            let drain_count = self.screen_buffer.len() - BUFFER_MAX_LINES;
            self.screen_buffer.drain(..drain_count);
        }
        self.last_poll = Some(Instant::now());
        self.polling = false;
        // Auto-follow: when active, every ingest pins the view to the bottom
        // so the user sees the freshest content by default.
        if self.auto_follow {
            self.scroll_offset = self.max_scroll();
        } else {
            // Manual mode — clamp so the offset can't go past the buffer end.
            let max_offset = self.max_scroll();
            if self.scroll_offset > max_offset {
                self.scroll_offset = max_offset;
            }
        }
    }

    /// Maximum valid scroll_offset.
    pub fn max_scroll(&self) -> u16 {
        (self.screen_buffer.len() as u16).saturating_sub(1)
    }

    // ── Key actions ────────────────────────────────────────────────────────────
    //
    // Any manual scroll disables auto-follow; only `go_bottom` re-enables it
    // because that's an explicit "snap to fresh" gesture.

    pub fn scroll_down(&mut self) {
        let max = self.max_scroll();
        if self.scroll_offset < max {
            self.scroll_offset = self.scroll_offset.saturating_add(3).min(max);
        }
        self.auto_follow = false;
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
        self.auto_follow = false;
    }

    /// Page-down (Space). Jumps PAGE_SIZE lines toward the bottom.
    pub fn page_down(&mut self) {
        let max = self.max_scroll();
        self.scroll_offset = self.scroll_offset.saturating_add(PAGE_SIZE).min(max);
        self.auto_follow = false;
    }

    /// Page-up (`-`). Jumps PAGE_SIZE lines toward the top.
    pub fn page_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(PAGE_SIZE);
        self.auto_follow = false;
    }

    pub fn go_top(&mut self) {
        self.scroll_offset = 0;
        self.auto_follow = false;
    }

    pub fn go_bottom(&mut self) {
        self.scroll_offset = self.max_scroll();
        self.auto_follow = true;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Agent rendering helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Returns `true` for roles that represent the human side of a conversation
/// (user turns are displayed verbatim; assistant turns are truncated).
pub fn is_user_role(role: &str) -> bool {
    matches!(role.to_ascii_lowercase().as_str(), "boyan" | "user")
}

/// Truncate `s` to at most `n` words (split by whitespace).
/// If truncated, appends `…` (single-character ellipsis).
pub fn truncate_words(s: &str, n: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= n {
        return s.to_string();
    }
    let head = words[..n].join(" ");
    format!("{head}…")
}

/// Re-read `session_path`, parse turns, and rebuild the peek buffer with the
/// truncation rule: user turns verbatim, assistant turns capped at 100 words.
///
/// This is a full replacement of `state.screen_buffer` (not an append).
pub fn rebuild_agent_buffer(state: &mut PeekState, session_path: &Path) {
    let text = match std::fs::read_to_string(session_path) {
        Ok(s) => s,
        Err(_) => {
            // Path is empty or unreadable (e.g. an agent surface whose
            // session.md hasn't been resolved yet). Clear polling so the
            // peek_tick doesn't deadlock and the render shows the
            // empty-buffer placeholder rather than "Loading screen…"
            // forever.
            state.polling = false;
            state.last_poll = Some(Instant::now());
            return;
        }
    };
    let turns = crate::mc_data::session_log::parse(&text);
    let mut buffer: Vec<String> = Vec::new();
    for turn in turns {
        let header = format!("## {} \u{2014} {}", turn.time, turn.role);
        buffer.push(header);
        let content = if is_user_role(&turn.role) {
            turn.content.clone()
        } else {
            truncate_words(&turn.content, 100)
        };
        for line in content.lines() {
            buffer.push(line.to_string());
        }
        buffer.push(String::new()); // blank separator between turns
    }
    // Replace existing buffer entirely.
    state.screen_buffer = buffer;
    state.last_poll = Some(Instant::now());
    state.polling = false;
    if state.auto_follow {
        state.scroll_offset = state.max_scroll();
    } else {
        let max = state.max_scroll();
        if state.scroll_offset > max {
            state.scroll_offset = max;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────────────

/// Render the peek-mode pane.
pub fn render(
    f: &mut Frame,
    area: Rect,
    peek: &PeekState,
    focused: bool,
    workspace_color: Option<&str>,
) {
    let border_style =
        crate::sidebar_pure::workspace_panel_border_style(workspace_color, focused, Color::Magenta);

    let title = format!(
        " Peek: {} (j/k slow · Space/- page · g/G top/bot · Esc back · Enter yield) ",
        peek.surface_label,
    );
    let block = Block::default()
        .title(Span::styled(title, border_style))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if peek.screen_buffer.is_empty() {
        let msg = if peek.polling {
            "Loading screen…"
        } else {
            "No screen data yet — yield via Enter to view, or wait for first poll."
        };
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let lines: Vec<Line> = peek
        .screen_buffer
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(Color::Gray))))
        .collect();

    // When auto-following, the goal is "the LAST N lines fill the visible
    // area, where N = inner.height". `peek.scroll_offset = max_scroll() =
    // buffer.len() - 1` was wrong — it scrolled past everything except the
    // final row, leaving the rest of the pane blank. Recompute the
    // effective scroll at render time so we can use the actual area height
    // (PeekState alone can't, because it doesn't know the area).
    let effective_offset = if peek.auto_follow {
        (peek.screen_buffer.len() as u16)
            .saturating_sub(inner.height)
    } else {
        peek.scroll_offset
    };

    let para = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((effective_offset, 0));
    f.render_widget(para, inner);
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    fn make_state() -> PeekState {
        PeekState::new(
            "workspace:1".to_string(),
            "workspace:3".to_string(),
            "workspace:3".to_string(),
            PeekSource::Shell,
        )
    }

    // ── scroll_down / scroll_up ───────────────────────────────────────────────

    #[test]
    fn scroll_down_increments_offset() {
        let mut ps = make_state();
        // Populate buffer with enough lines to scroll.
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_down();
        assert_eq!(ps.scroll_offset, 3);
    }

    #[test]
    fn scroll_up_decrements_offset() {
        let mut ps = make_state();
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 10;
        ps.scroll_up();
        assert_eq!(ps.scroll_offset, 7);
    }

    #[test]
    fn scroll_up_clamps_at_zero() {
        let mut ps = make_state();
        for i in 0..20 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 2;
        ps.scroll_up();
        // 2 - 3 saturates to 0
        assert_eq!(ps.scroll_offset, 0);
    }

    // ── g / G ─────────────────────────────────────────────────────────────────

    #[test]
    fn go_top_resets_to_zero() {
        let mut ps = make_state();
        for i in 0..30 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 15;
        ps.go_top();
        assert_eq!(ps.scroll_offset, 0);
    }

    #[test]
    fn go_bottom_goes_to_max_scroll() {
        let mut ps = make_state();
        for i in 0..30 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.go_bottom();
        assert_eq!(ps.scroll_offset, ps.max_scroll());
        assert_eq!(ps.scroll_offset, 29);
    }

    // ── ingest / rolling buffer ───────────────────────────────────────────────

    #[test]
    fn ingest_screen_appends_lines() {
        let mut ps = make_state();
        ps.ingest_screen("line A\nline B\nline C\n");
        assert_eq!(ps.screen_buffer.len(), 3);
        assert_eq!(ps.screen_buffer[0], "line A");
        assert_eq!(ps.screen_buffer[2], "line C");
    }

    #[test]
    fn buffer_truncates_to_max_lines() {
        let mut ps = make_state();
        // Fill well past BUFFER_MAX_LINES.
        for i in 0..BUFFER_MAX_LINES + 100 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        // Now ingest one more.
        ps.ingest_screen("new line\n");
        assert!(ps.screen_buffer.len() <= BUFFER_MAX_LINES);
    }

    #[test]
    fn last_poll_set_after_ingest() {
        let mut ps = make_state();
        assert!(ps.last_poll.is_none());
        ps.ingest_screen("hello\n");
        assert!(ps.last_poll.is_some());
    }

    // ── render ────────────────────────────────────────────────────────────────

    fn buf_dump(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_shows_loading_when_buffer_empty() {
        let ps = make_state();
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 10), &ps, true, None))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("No screen data") || dump.contains("Loading"),
            "expected loading message, got: {dump}"
        );
    }

    #[test]
    fn render_shows_surface_id_in_title() {
        let mut ps = PeekState::new(
            "workspace:1".to_string(),
            "workspace:5".to_string(),
            "my-surface".to_string(),
            PeekSource::Shell,
        );
        ps.ingest_screen("hello world\n");
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 10), &ps, true, None))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("my-surface"),
            "surface label not in title: {dump}"
        );
    }

    #[test]
    fn render_shows_screen_buffer_content() {
        let mut ps = make_state();
        // Disable auto-follow so the view starts at the top — keeps this
        // test focused on "the renderer actually emits buffer content"
        // regardless of where the cursor sits.
        ps.auto_follow = false;
        ps.ingest_screen("hello from peek\nworld line\n");
        ps.scroll_offset = 0;
        let backend = TestBackend::new(80, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 15), &ps, true, None))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("hello from peek"),
            "screen content not shown: {dump}"
        );
    }

    #[test]
    fn render_with_auto_follow_shows_bottom_of_buffer() {
        let mut ps = make_state();
        ps.ingest_screen("hello from peek\nworld line\n");
        // auto_follow is on by default — ingest should snap to bottom.
        let backend = TestBackend::new(80, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 5), &ps, true, None))
            .unwrap();
        let dump = buf_dump(&terminal);
        // With a 5-row pane (3 inner rows) and 2 lines of content scrolled to
        // the bottom, "world line" should be visible — that's the newest.
        assert!(dump.contains("world line"), "bottom line not shown: {dump}");
    }

    // ── Auto-follow + pagination ────────────────────────────────────────────

    #[test]
    fn auto_follow_initially_true() {
        let ps = make_state();
        assert!(
            ps.auto_follow,
            "auto_follow should default to true on new()"
        );
    }

    #[test]
    fn ingest_with_auto_follow_snaps_to_bottom() {
        let mut ps = make_state();
        for i in 0..30 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 0;
        ps.auto_follow = true;
        // simulate a new ingest that adds more content
        ps.ingest_screen("new-line-a\nnew-line-b\n");
        // After ingest, scroll_offset should be at the buffer's max (bottom).
        assert_eq!(ps.scroll_offset, ps.max_scroll());
        assert!(
            ps.auto_follow,
            "ingest must not disable auto_follow on its own"
        );
    }

    #[test]
    fn ingest_without_auto_follow_preserves_offset() {
        let mut ps = make_state();
        for i in 0..30 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 5;
        ps.auto_follow = false;
        ps.ingest_screen("new-line\n");
        // Offset stays at 5 (clamped within range, which it is).
        assert_eq!(ps.scroll_offset, 5);
    }

    #[test]
    fn manual_scroll_down_disables_auto_follow() {
        let mut ps = make_state();
        for i in 0..30 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        assert!(ps.auto_follow);
        ps.scroll_down();
        assert!(!ps.auto_follow, "j must disable auto_follow");
    }

    #[test]
    fn page_down_moves_by_page_size() {
        let mut ps = make_state();
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 0;
        ps.page_down();
        assert_eq!(ps.scroll_offset, PAGE_SIZE);
        assert!(!ps.auto_follow);
    }

    #[test]
    fn page_down_clamps_at_bottom() {
        let mut ps = make_state();
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        let max = ps.max_scroll();
        ps.scroll_offset = max.saturating_sub(2);
        ps.page_down();
        assert_eq!(ps.scroll_offset, max);
    }

    #[test]
    fn page_up_moves_by_page_size() {
        let mut ps = make_state();
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 25;
        ps.page_up();
        assert_eq!(ps.scroll_offset, 25 - PAGE_SIZE);
        assert!(!ps.auto_follow);
    }

    #[test]
    fn page_up_saturates_at_zero() {
        let mut ps = make_state();
        for i in 0..50 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_offset = 3;
        ps.page_up();
        assert_eq!(ps.scroll_offset, 0);
    }

    #[test]
    fn go_bottom_reenables_auto_follow() {
        let mut ps = make_state();
        for i in 0..30 {
            ps.screen_buffer.push(format!("line {i}"));
        }
        ps.scroll_up(); // disables auto_follow
        assert!(!ps.auto_follow);
        ps.go_bottom();
        assert!(ps.auto_follow, "G must re-enable auto_follow");
        assert_eq!(ps.scroll_offset, ps.max_scroll());
    }

    // ── Agent rendering helpers ───────────────────────────────────────────────

    #[test]
    fn truncate_words_truncates_after_n_words() {
        let s = "one two three four five six seven eight nine ten eleven";
        let result = truncate_words(s, 5);
        assert_eq!(result, "one two three four five…");
    }

    #[test]
    fn truncate_words_passes_through_short_content() {
        let s = "hello world";
        let result = truncate_words(s, 100);
        // Fewer than 100 words — returned verbatim (same string value).
        assert_eq!(result, s);
    }

    #[test]
    fn is_user_role_matches_boyan_and_user_case_insensitive() {
        assert!(is_user_role("boyan"));
        assert!(is_user_role("Boyan"));
        assert!(is_user_role("BOYAN"));
        assert!(is_user_role("user"));
        assert!(is_user_role("User"));
        assert!(!is_user_role("claude"));
        assert!(!is_user_role("assistant"));
        assert!(!is_user_role("codex"));
    }

    #[test]
    fn rebuild_agent_buffer_assembles_turns_with_truncation() {
        // Build a session log where the assistant turn has > 100 words.
        let long_response: String = (0..120)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let log = format!(
            "---\nworkspace_id: test\n---\n\n## 09:00 PT \u{2014} boyan\nhello there\n\n---\n\n## 09:01 PT \u{2014} claude\n{long_response}\n"
        );
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &log).unwrap();

        let mut ps = PeekState::new(
            "workspace:1".to_string(),
            "surface:1".to_string(),
            "test".to_string(),
            PeekSource::Agent {
                session_path: tmp.path().to_path_buf(),
            },
        );
        rebuild_agent_buffer(&mut ps, tmp.path());

        // Buffer should not be empty.
        assert!(!ps.screen_buffer.is_empty());

        // Find the line that starts with "## 09:01" — that's the assistant header.
        let assistant_header = ps
            .screen_buffer
            .iter()
            .find(|l| l.contains("09:01"))
            .unwrap();
        assert!(
            assistant_header.contains("claude"),
            "expected claude in header: {assistant_header}"
        );

        // The content line after the assistant header should contain "…" (truncated).
        let assistant_header_idx = ps
            .screen_buffer
            .iter()
            .position(|l| l.contains("09:01"))
            .unwrap();
        let content_after = ps.screen_buffer[assistant_header_idx + 1..]
            .iter()
            .find(|l| !l.is_empty())
            .unwrap();
        assert!(
            content_after.contains('…'),
            "expected ellipsis in truncated turn: {content_after}"
        );
    }

    #[test]
    fn rebuild_agent_buffer_preserves_user_turns_verbatim() {
        let user_text = "please do the thing exactly as I described";
        let log =
            format!("---\nworkspace_id: test\n---\n\n## 10:00 PT \u{2014} boyan\n{user_text}\n");
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &log).unwrap();

        let mut ps = PeekState::new(
            "workspace:1".to_string(),
            "surface:1".to_string(),
            "test".to_string(),
            PeekSource::Agent {
                session_path: tmp.path().to_path_buf(),
            },
        );
        rebuild_agent_buffer(&mut ps, tmp.path());

        // The buffer should contain the user's text verbatim.
        let joined = ps.screen_buffer.join("\n");
        assert!(
            joined.contains(user_text),
            "user text not verbatim in buffer: {joined}"
        );
        // No ellipsis — user turn is short.
        assert!(
            !joined.contains('…'),
            "user turn should not be truncated: {joined}"
        );
    }
}
