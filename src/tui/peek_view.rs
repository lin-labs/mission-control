/// Peek mode: read-only screen view for a surface.
///
/// Activated by pressing Enter on a `## Current surfaces` row in nav mode.
/// Shows a live (polled) read of the surface's screen content.
/// Keys: j/k scroll · g/G top/bottom · Esc back · Enter yield (select workspace).
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::time::Instant;

// ──────────────────────────────────────────────────────────────────────────────
// State
// ──────────────────────────────────────────────────────────────────────────────

/// Maximum number of lines to keep in the rolling screen buffer.
pub const BUFFER_MAX_LINES: usize = 500;

/// Interval between screen polls while in peek mode.
pub const PEEK_POLL_INTERVAL_SECS: u64 = 1;

/// State for peek mode (reading a surface's screen).
#[derive(Debug, Clone)]
pub struct PeekState {
    /// The surface/workspace reference ID used to call `read_screen`.
    /// This is the workspace `ref_id` (e.g., "workspace:3") since
    /// `cmux read-screen` takes a workspace ref, not a surface id.
    pub surface_ref: String,
    /// Human-readable label shown in the peek title bar.
    pub surface_label: String,
    /// How many lines from the top of the buffer to display.
    pub scroll_offset: u16,
    /// Rolling buffer of recent screen content (~500 lines max).
    pub screen_buffer: Vec<String>,
    /// When we last polled the screen.
    pub last_poll: Option<Instant>,
    /// Whether we're currently waiting for a poll to complete.
    pub polling: bool,
}

impl PeekState {
    pub fn new(surface_ref: String, surface_label: String) -> Self {
        Self {
            surface_ref,
            surface_label,
            scroll_offset: 0,
            screen_buffer: Vec::new(),
            last_poll: None,
            polling: false,
        }
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
        // Clamp scroll_offset so it can't go past the buffer end.
        let max_offset = self.max_scroll();
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
    }

    /// Maximum valid scroll_offset.
    pub fn max_scroll(&self) -> u16 {
        (self.screen_buffer.len() as u16).saturating_sub(1)
    }

    // ── Key actions ────────────────────────────────────────────────────────────

    pub fn scroll_down(&mut self) {
        let max = self.max_scroll();
        if self.scroll_offset < max {
            self.scroll_offset = self.scroll_offset.saturating_add(3).min(max);
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
    }

    pub fn go_top(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn go_bottom(&mut self) {
        self.scroll_offset = self.max_scroll();
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────────────

/// Render the peek-mode pane.
pub fn render(f: &mut Frame, area: Rect, peek: &PeekState, focused: bool) {
    let border_color = if focused { Color::Magenta } else { Color::DarkGray };

    let title = format!(
        " Peek: {} (j/k scroll · g/G top/bot · Esc back · Enter yield) ",
        peek.surface_label,
    );
    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

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

    let para = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((peek.scroll_offset, 0));
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
        PeekState::new("workspace:3".to_string(), "workspace:3".to_string())
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
                        buf.cell((x, y)).map(|c| c.symbol().chars().next().unwrap_or(' '))
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
            .draw(|f| render(f, Rect::new(0, 0, 80, 10), &ps, true))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("No screen data") || dump.contains("Loading"),
            "expected loading message, got: {dump}"
        );
    }

    #[test]
    fn render_shows_surface_id_in_title() {
        let mut ps = PeekState::new("workspace:5".to_string(), "my-surface".to_string());
        ps.ingest_screen("hello world\n");
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 10), &ps, true))
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
        ps.ingest_screen("hello from peek\nworld line\n");
        let backend = TestBackend::new(80, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 15), &ps, true))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("hello from peek"),
            "screen content not shown: {dump}"
        );
    }
}
