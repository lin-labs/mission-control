use crate::tui::app::Focus;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// Render the keyboard-shortcut footer line at the bottom of the screen.
/// Hints adapt slightly based on which pane currently has focus.
pub fn render_footer(f: &mut Frame, area: Rect, focus: Focus) {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Gray);
    let sep_style = Style::default().fg(Color::DarkGray);

    let mut spans: Vec<Span> = Vec::new();

    let pairs: &[(&str, &str)] = match focus {
        Focus::Sidebar => &[
            ("j/k", "navigate"),
            ("l/⏎", "open detail"),
            ("⏎", "switch ws"),
            ("s", "rescreen"),
            ("r", "summarize"),
            ("^r", "reload"),
            ("n", "notes"),
            ("q", "quit"),
        ],
        Focus::Detail => &[
            ("j/k", "scroll"),
            ("h/esc", "back"),
            ("⏎", "switch ws"),
            ("s", "rescreen"),
            ("r", "summarize"),
            ("^r", "reload"),
            ("n", "notes"),
            ("q", "quit"),
        ],
    };

    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", sep_style));
        }
        spans.push(Span::styled(format!(" {} ", key), key_style));
        spans.push(Span::styled(format!(" {}", label), label_style));
    }

    let paragraph = Paragraph::new(Line::from(spans));
    f.render_widget(paragraph, area);
}
