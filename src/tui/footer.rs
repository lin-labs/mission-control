use crate::tui::app::Focus;
use crate::tui::command::{CommandLine, StatusLine};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// Render the keyboard-shortcut footer line.
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
            (":", "cmd"),
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
            (":", "cmd"),
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

/// Render the `:command` bar in place of the keybind footer.
///
/// Layout: `:<buffer><cursor><ghost-dim>    <status>`
/// Status is appended after a 4-space gap when present.
pub fn render_command_bar(f: &mut Frame, area: Rect, cl: &CommandLine) {
    let prompt_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let buf_style = Style::default();
    let ghost_style = Style::default().fg(Color::DarkGray);

    let (before, after): (&str, &str) = cl.buffer.split_at(cl.cursor.min(cl.buffer.len()));

    // Render the cursor as an inverted single char. If the cursor is at the
    // end, render a space so the user sees a block where they're about to type.
    let (cursor_char, rest_after): (String, &str) = if after.is_empty() {
        (" ".to_string(), "")
    } else {
        let mut chars = after.char_indices();
        let (_, c) = chars.next().unwrap();
        let rest_start = chars.next().map(|(i, _)| i).unwrap_or(after.len());
        (c.to_string(), &after[rest_start..])
    };

    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    let mut spans = vec![
        Span::styled(":", prompt_style),
        Span::styled(before.to_string(), buf_style),
        Span::styled(cursor_char, cursor_style),
        Span::styled(rest_after.to_string(), buf_style),
        Span::styled(cl.ghost().to_string(), ghost_style),
    ];

    if let Some(ref status) = cl.status {
        spans.push(Span::raw("    "));
        match status {
            StatusLine::Ok(msg) => {
                spans.push(Span::styled(
                    format!("✓ {}", msg),
                    Style::default().fg(Color::Green),
                ));
            }
            StatusLine::Err(msg) => {
                spans.push(Span::styled(
                    format!("✗ {}", msg),
                    Style::default().fg(Color::Red),
                ));
            }
        }
    }

    let paragraph = Paragraph::new(Line::from(spans));
    f.render_widget(paragraph, area);
}
