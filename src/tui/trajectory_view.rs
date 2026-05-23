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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const SAMPLE: &str = "---
workspace: predinvest
---

## Goal
- Build self-improvement-enabled investment agent

## Current surfaces
- claude · mbp · working · writing tests              <!-- mc:surface:sid-1 -->

## Tasks & Progress
- [x] sprint-01 done
- [ ] sprint-02
";

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
    fn render_emits_section_headers_and_items() {
        let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 20), Some(&doc), 0, false))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(dump.contains("Goal"), "missing Goal header: {dump}");
        assert!(dump.contains("Current surfaces"), "missing Current surfaces header");
        assert!(dump.contains("Tasks & Progress"), "missing Tasks header");
        assert!(dump.contains("Build self-improvement"), "missing Goal item");
        assert!(dump.contains("writing tests"), "missing surface text");
        assert!(dump.contains("sprint-01 done"), "missing task text");
        assert!(!dump.contains("mc:surface:"), "leaked surface comment into UI");
    }

    #[test]
    fn render_with_no_doc_shows_placeholder() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 10), None, 0, false))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(dump.contains("No trajectory") || dump.contains("no trajectory"));
    }
}
