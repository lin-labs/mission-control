use crate::mc_data::trajectory::TrajectoryDoc;
use crate::tui::peek_view::PeekState;
use crate::tui::trajectory_edit::{EditMode, InsertFocus, TrajectoryEditState};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

/// Render the trajectory detail pane.
///
/// `edit_state` is `None` when no editing session is active for this workspace.
/// `peek_state` is `Some` when peek mode is active; in that case the pane is
/// entirely replaced by the peek screen view.
pub fn render(
    f: &mut Frame,
    area: Rect,
    doc: Option<&TrajectoryDoc>,
    scroll: u16,
    focused: bool,
    edit_state: Option<&TrajectoryEditState>,
    peek_state: Option<&PeekState>,
) {
    // If in peek mode, delegate entirely to peek_view.
    if let Some(peek) = peek_state {
        crate::tui::peek_view::render(f, area, peek, focused);
        return;
    }
    let in_insert = edit_state
        .map(|s| matches!(s.mode, EditMode::Insert { .. }))
        .unwrap_or(false);

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

    // When in insert mode split the pane: body on top, input-ctx strip at bottom.
    let (body_area, maybe_ctx_area) = if in_insert {
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(5), // separator + 3 lines + blank
        ])
        .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    // ── Body block ───────────────────────────────────────────────────────────
    let body_block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let body_inner = body_block.inner(body_area);
    f.render_widget(body_block, body_area);

    let mut lines: Vec<Line> = Vec::new();
    for (sec_idx, section) in doc.sections.iter().enumerate() {
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
            for (item_idx, item) in section.items.iter().enumerate() {
                let is_cursor = edit_state
                    .map(|s| s.cursor_section == sec_idx && s.cursor_item == item_idx)
                    .unwrap_or(false);
                let is_insert_cursor = is_cursor && in_insert;

                // When this item is the one being edited, use the buffer text.
                let display_text: &str = if is_insert_cursor {
                    edit_state.map(|s| s.edit_buffer.as_str()).unwrap_or(&item.text)
                } else {
                    &item.text
                };

                let prefix = if item.is_checkbox {
                    if item.checked.unwrap_or(false) {
                        "- [x] "
                    } else {
                        "- [ ] "
                    }
                } else {
                    "- "
                };
                let text_color = if item.is_checkbox && item.checked.unwrap_or(false) {
                    Color::DarkGray
                } else {
                    Color::Gray
                };

                let line = if is_cursor && !in_insert {
                    // Nav mode cursor: highlight with Cyan background.
                    Line::from(Span::styled(
                        format!("{prefix}{display_text}"),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan),
                    ))
                } else if is_insert_cursor {
                    // Insert mode cursor: render with a block cursor at cursor_col.
                    // Split: prefix + chars[..cursor_col] | cursor char | chars[cursor_col+1..]
                    let cursor_col = edit_state.map(|s| s.cursor_col).unwrap_or(0);
                    let chars: Vec<char> = display_text.chars().collect();
                    let before: String = chars[..cursor_col.min(chars.len())].iter().collect();
                    let cursor_char: String = if cursor_col < chars.len() {
                        chars[cursor_col].to_string()
                    } else {
                        " ".to_string()
                    };
                    let after: String = if cursor_col + 1 < chars.len() {
                        chars[cursor_col + 1..].iter().collect()
                    } else {
                        String::new()
                    };
                    Line::from(vec![
                        Span::styled(
                            format!("{prefix}{before}"),
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            cursor_char,
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Yellow),
                        ),
                        Span::styled(
                            after,
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                } else {
                    Line::from(Span::styled(
                        format!("{prefix}{display_text}"),
                        Style::default().fg(text_color),
                    ))
                };
                lines.push(line);
            }
        }
        lines.push(Line::raw(""));
    }

    let para = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, body_inner);

    // ── Input context strip (only in insert mode) ────────────────────────────
    if let Some(ctx_area) = maybe_ctx_area {
        let ctx_block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));
        let ctx_inner = ctx_block.inner(ctx_area);
        f.render_widget(ctx_block, ctx_area);

        let ctx_buf = edit_state.map(|s| s.input_ctx_buffer.as_str()).unwrap_or("");
        let focus_on_ctx = edit_state
            .map(|s| matches!(s.mode, EditMode::Insert { focus: InsertFocus::InputCtx }))
            .unwrap_or(false);

        let separator_style = Style::default().fg(Color::DarkGray);
        let sep_label = if focus_on_ctx {
            "─── input context (Esc to save) ───────────────"
        } else {
            "─── input context (Tab to switch, Esc to save) "
        };

        let cursor_char = if focus_on_ctx { "▋" } else { "" };
        let mut ctx_lines: Vec<Line> = vec![
            Line::from(Span::styled(sep_label, separator_style)),
            Line::from(vec![
                Span::styled(ctx_buf, Style::default().fg(Color::White)),
                Span::styled(cursor_char, Style::default().fg(Color::Yellow)),
            ]),
        ];
        if ctx_buf.is_empty() && !focus_on_ctx {
            ctx_lines.push(Line::from(Span::styled(
                "  (Tab to add context for this edit)",
                Style::default().fg(Color::DarkGray),
            )));
        }

        let ctx_para = Paragraph::new(Text::from(ctx_lines));
        f.render_widget(ctx_para, ctx_inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::trajectory_edit::TrajectoryEditState;
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
            .draw(|f| render(f, Rect::new(0, 0, 80, 20), Some(&doc), 0, false, None, None))
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
            .draw(|f| render(f, Rect::new(0, 0, 80, 10), None, 0, false, None, None))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(dump.contains("No trajectory") || dump.contains("no trajectory"));
    }

    #[test]
    fn render_highlights_cursor_item_in_nav_mode() {
        let mut doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        doc.ensure_sections();
        let state = TrajectoryEditState {
            cursor_section: 0,
            cursor_item: 0,
            mode: EditMode::Nav,
            ..Default::default()
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 24), Some(&doc), 0, true, Some(&state), None))
            .unwrap();
        let buf = terminal.backend().buffer();
        // The cursor cell (first item in Goal) should have a Cyan background.
        // Find the first row that contains "Build self-improvement" and check BG color.
        let mut found = false;
        'outer: for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().chars().next().unwrap_or(' ')))
                .collect();
            if row.contains("Build self-improvement") {
                // Check the background color of one of those cells.
                for x in 0..buf.area.width {
                    if let Some(cell) = buf.cell((x, y)) {
                        if cell.symbol() == "B" {
                            assert_eq!(
                                cell.style().bg,
                                Some(Color::Cyan),
                                "cursor item should have Cyan bg"
                            );
                            found = true;
                            break 'outer;
                        }
                    }
                }
            }
        }
        assert!(found, "did not find highlighted cursor item");
    }

    #[test]
    fn render_insert_mode_shows_input_context_strip() {
        let mut doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        doc.ensure_sections();
        let state = TrajectoryEditState {
            cursor_section: 0,
            cursor_item: 0,
            mode: EditMode::Insert { focus: InsertFocus::Item },
            edit_buffer: "Edited goal".to_string(),
            ..Default::default()
        };
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 30), Some(&doc), 0, true, Some(&state), None))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(dump.contains("input context"), "missing input context strip");
        assert!(dump.contains("Edited goal"), "buffer text not shown");
    }

    #[test]
    fn render_insert_mode_shows_edit_buffer_text() {
        let mut doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        doc.ensure_sections();
        let state = TrajectoryEditState {
            cursor_section: 2, // Tasks
            cursor_item: 1,    // sprint-02
            mode: EditMode::Insert { focus: InsertFocus::Item },
            edit_buffer: "sprint-02 in progress".to_string(),
            ..Default::default()
        };
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 30), Some(&doc), 0, true, Some(&state), None))
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("sprint-02 in progress"),
            "edit buffer text not shown: {dump}"
        );
    }
}
