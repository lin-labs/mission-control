use crate::mc_data::trajectory::TrajectoryDoc;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

pub fn render(
    f: &mut Frame,
    area: Rect,
    doc: Option<&TrajectoryDoc>,
    scroll: u16,
    focused: bool,
) {
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let doc = match doc {
        Some(d) => d,
        None => {
            f.render_widget(
                Paragraph::new("No trajectory yet for this workspace.").block(block),
                area,
            );
            return;
        }
    };

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for section in &doc.sections {
        lines.push(Line::from(Span::styled(
            format!("## {}", section.name),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        if section.items.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (empty)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for item in &section.items {
                let prefix = if item.is_checkbox {
                    if item.checked.unwrap_or(false) {
                        "- [x] "
                    } else {
                        "- [ ] "
                    }
                } else {
                    "- "
                };
                let color = if item.is_checkbox && item.checked.unwrap_or(false) {
                    Color::DarkGray
                } else {
                    Color::Gray
                };
                lines.push(Line::from(Span::styled(
                    format!("{prefix}{}", item.text),
                    Style::default().fg(color),
                )));
            }
        }
        lines.push(Line::raw(""));
    }

    let para = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, inner);
}
