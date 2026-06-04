use std::collections::HashSet;

use crate::mc_data::surface_kind::SurfaceKind;
use crate::mc_data::trajectory::{SECTION_CURRENT_SURFACES, SECTION_GOALS, Section, TrajectoryDoc};
use crate::tui::peek_view::PeekState;
use crate::tui::trajectory_edit::{EditMode, InsertFocus, TrajectoryEditState};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

/// Map a kind to the text color used for its glyph + label in surface rows.
pub fn kind_color(kind: SurfaceKind) -> Color {
    match kind {
        SurfaceKind::Claude => Color::Rgb(217, 119, 6),
        SurfaceKind::Codex => Color::Rgb(6, 182, 212),
        SurfaceKind::OtherAgent => Color::Magenta,
        SurfaceKind::Shell => Color::Gray,
        SurfaceKind::Unknown => Color::DarkGray,
    }
}

/// Recognize a row-leading character as a kind glyph.
///
/// trajectory rows in `## Current surfaces` are written by
/// `surface_render::format_surface_text` and always start with one of these.
fn glyph_kind(c: char) -> Option<SurfaceKind> {
    match c {
        '✻' => Some(SurfaceKind::Claude),
        '▲' => Some(SurfaceKind::Codex),
        '◆' => Some(SurfaceKind::OtherAgent),
        '$' => Some(SurfaceKind::Shell),
        '·' => Some(SurfaceKind::Unknown),
        _ => None,
    }
}

/// Split a surface-row text into 3 styled spans:
/// (glyph_label_span, body_span, badge_span). If a `← goal:` badge is
/// present it gets the third span styled as DarkGray; the leading
/// `{glyph} {label} · ` slice gets the kind color. When `dim` is set the
/// glyph+label span carries `Modifier::DIM` so a "just-exited" agent
/// surface still renders with the agent glyph but visually de-emphasized.
fn surface_row_spans<'a>(text: &'a str, dim: bool, base_style: Style) -> Vec<Span<'a>> {
    // Find the first whitespace char (between glyph and label).
    let mut chars = text.chars();
    let first = chars.next();
    let kind = first.and_then(glyph_kind);

    let (head_end, badge_start) = {
        // head = `{glyph} {label} · ` if the row matches the format.
        // Find the FIRST " · " separator (between label and rest-of-title).
        let head_end = text.find(" · ").map(|i| i + " · ".len()).unwrap_or(0);
        let badge_start = surface_annotation_start(text);
        (head_end.min(badge_start), badge_start)
    };

    let mut spans = Vec::new();
    if let Some(kind) = kind {
        let mut glyph_style = base_style.fg(kind_color(kind));
        if dim {
            glyph_style = glyph_style.add_modifier(Modifier::DIM);
        }
        spans.push(Span::styled(&text[..head_end], glyph_style));
        spans.push(Span::styled(&text[head_end..badge_start], base_style));
    } else {
        // Row doesn't lead with a known glyph → render as a plain row.
        spans.push(Span::styled(&text[..badge_start], base_style));
    }
    if badge_start < text.len() {
        spans.push(Span::styled(
            &text[badge_start..],
            base_style.fg(Color::DarkGray),
        ));
    }
    spans
}

fn surface_annotation_start(text: &str) -> usize {
    ["   ← goal:", "   overall:", "   ask:"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
        .unwrap_or(text.len())
}

fn split_surface_intent(text: &str) -> (&str, Option<&str>, Option<&str>) {
    let overall_marker = "   overall:";
    let ask_marker = "   ask:";
    let overall_idx = text.find(overall_marker);
    let ask_idx = text.find(ask_marker);
    let main_end = [overall_idx, ask_idx]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(text.len());

    let overall = overall_idx.map(|idx| {
        let start = idx + overall_marker.len();
        let end = ask_idx.filter(|ask| *ask > start).unwrap_or(text.len());
        text[start..end].trim()
    });
    let ask = ask_idx.map(|idx| {
        let start = idx + ask_marker.len();
        text[start..].trim()
    });

    (text[..main_end].trim_end(), overall, ask)
}

fn surface_item_lines<'a>(prefix: &str, text: &'a str, dim: bool, base: Style) -> Vec<Line<'a>> {
    let (main, overall, ask) = split_surface_intent(text);
    let mut first = vec![Span::styled(prefix.to_string(), base)];
    first.extend(surface_row_spans(main, dim, base));
    let mut lines = vec![Line::from(first)];
    if let Some(goal) = overall.filter(|s| !s.is_empty()) {
        lines.push(Line::from(vec![
            Span::styled("    overall: ", base.fg(Color::DarkGray)),
            Span::styled(goal, base.fg(Color::Gray)),
        ]));
    }
    if let Some(ask) = ask.filter(|s| !s.is_empty()) {
        lines.push(Line::from(vec![
            Span::styled("    latest:  ", base.fg(Color::DarkGray)),
            Span::styled(ask, base.fg(Color::Gray)),
        ]));
    }
    lines
}

/// Build spans for a Beads row, splitting off any trailing
/// `   → <glyph> <surface_ref>` badge so it can be DarkGray.
fn goal_row_spans<'a>(text: &'a str, base_style: Style) -> Vec<Span<'a>> {
    let badge_start = text.find("   → ").unwrap_or(text.len());
    let mut spans = Vec::new();
    spans.push(Span::styled(&text[..badge_start], base_style));
    if badge_start < text.len() {
        spans.push(Span::styled(
            &text[badge_start..],
            base_style.fg(Color::DarkGray),
        ));
    }
    spans
}

fn is_goals_section(section: &Section) -> bool {
    section.name == SECTION_GOALS
}

/// Per-section, per-item context for dim-glyph decisions. Empty by default.
#[derive(Default, Debug, Clone)]
pub struct RenderHints {
    /// Set of surface refs whose glyph should be rendered with Modifier::DIM
    /// (current kind is Shell/Unknown but effective_kind elevated it from a
    /// fresh last-agent snapshot).
    pub dim_surface_refs: HashSet<String>,
}

/// Render the trajectory detail pane.
///
/// `edit_state` is `None` when no editing session is active for this workspace.
/// `peek_state` is `Some` when peek mode is active; in that case the pane is
/// entirely replaced by the peek screen view.
///
/// Wrapper kept for the existing test surface; binary callers use
/// `render_with_hints`.
#[allow(dead_code)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    doc: Option<&TrajectoryDoc>,
    scroll: u16,
    focused: bool,
    edit_state: Option<&TrajectoryEditState>,
    peek_state: Option<&PeekState>,
    workspace_color: Option<&str>,
) {
    render_with_hints(
        f,
        area,
        doc,
        scroll,
        focused,
        edit_state,
        peek_state,
        workspace_color,
        &RenderHints::default(),
    )
}

/// Variant of `render` that accepts per-surface styling hints. Existing
/// call sites (and tests) keep using `render`; the binary uses this from
/// `detail.rs` so it can plumb the dim-glyph hint set from
/// `WorkspaceState.surfaces`.
pub fn render_with_hints(
    f: &mut Frame,
    area: Rect,
    doc: Option<&TrajectoryDoc>,
    scroll: u16,
    focused: bool,
    edit_state: Option<&TrajectoryEditState>,
    peek_state: Option<&PeekState>,
    workspace_color: Option<&str>,
    hints: &RenderHints,
) {
    // If in peek mode, delegate entirely to peek_view.
    if let Some(peek) = peek_state {
        crate::tui::peek_view::render(f, area, peek, focused, workspace_color);
        return;
    }
    let in_insert = edit_state
        .map(|s| matches!(s.mode, EditMode::Insert { .. }))
        .unwrap_or(false);

    let border_style =
        crate::sidebar_pure::workspace_panel_border_style(workspace_color, focused, Color::Cyan);
    let block = Block::default()
        .title(Span::styled(" Detail ", border_style))
        .borders(Borders::ALL)
        .border_style(border_style);

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
        .title(Span::styled(" Detail ", border_style))
        .borders(Borders::ALL)
        .border_style(border_style);
    let body_inner = body_block.inner(body_area);
    f.render_widget(body_block, body_area);

    let mut lines: Vec<Line> = Vec::new();
    for (sec_idx, section) in doc.sections.iter().enumerate() {
        // Determine whether the cursor is "on" this section's header, which
        // happens when the section is empty and cursor_section == sec_idx.
        let is_header_cursor = edit_state
            .map(|s| {
                s.cursor_section == sec_idx
                    && s.cursor_item == 0
                    && section.items.is_empty()
                    && matches!(s.mode, EditMode::Nav)
            })
            .unwrap_or(false);

        let header_line = if is_header_cursor {
            Line::from(Span::styled(
                format!("## {}", section.name),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(Span::styled(
                format!("## {}", section.name),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ))
        };
        lines.push(header_line);
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
                    edit_state
                        .map(|s| s.edit_buffer.as_str())
                        .unwrap_or(&item.text)
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
                if section.name == SECTION_CURRENT_SURFACES && !is_cursor && !is_insert_cursor {
                    let dim = item
                        .surface_id
                        .as_deref()
                        .map(|sid| hints.dim_surface_refs.contains(sid))
                        .unwrap_or(false);
                    let base = Style::default().fg(text_color);
                    lines.extend(surface_item_lines(prefix, display_text, dim, base));
                    continue;
                }

                let line = if is_cursor && !in_insert {
                    // Nav mode cursor: highlight with Cyan background.
                    Line::from(Span::styled(
                        format!("{prefix}{display_text}"),
                        Style::default().fg(Color::Black).bg(Color::Cyan),
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
                            Style::default().fg(Color::Black).bg(Color::Yellow),
                        ),
                        Span::styled(
                            after,
                            Style::default()
                                .fg(Color::White)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                } else {
                    // Non-cursor, non-insert path: split the row into styled
                    // spans for surface rows (glyph + label colored by kind,
                    // optional `← goal:` badge dimmed) and goal rows (with
                    // an optional `→ <glyph> <ref>` badge).
                    let base = Style::default().fg(text_color);
                    if section.name == SECTION_CURRENT_SURFACES {
                        let dim = item
                            .surface_id
                            .as_deref()
                            .map(|sid| hints.dim_surface_refs.contains(sid))
                            .unwrap_or(false);
                        let mut spans = vec![Span::styled(prefix.to_string(), base)];
                        spans.extend(surface_row_spans(display_text, dim, base));
                        Line::from(spans)
                    } else if is_goals_section(section) {
                        let mut spans = vec![Span::styled(prefix.to_string(), base)];
                        spans.extend(goal_row_spans(display_text, base));
                        Line::from(spans)
                    } else {
                        Line::from(Span::styled(format!("{prefix}{display_text}"), base))
                    }
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

        let ctx_buf = edit_state
            .map(|s| s.input_ctx_buffer.as_str())
            .unwrap_or("");
        let focus_on_ctx = edit_state
            .map(|s| {
                matches!(
                    s.mode,
                    EditMode::Insert {
                        focus: InsertFocus::InputCtx
                    }
                )
            })
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

## Mission
- Build self-improvement-enabled investment agent

## Current surfaces
- claude · mbp · working · writing tests              <!-- mc:surface:sid-1 -->

## Beads
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
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 80, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    None,
                )
            })
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(dump.contains("Mission"), "missing Mission header: {dump}");
        assert!(
            dump.contains("Current surfaces"),
            "missing Current surfaces header"
        );
        assert!(dump.contains("Beads"), "missing Beads header");
        assert!(
            dump.contains("Build self-improvement"),
            "missing Mission item"
        );
        assert!(dump.contains("writing tests"), "missing surface text");
        assert!(dump.contains("sprint-01 done"), "missing task text");
        assert!(
            !dump.contains("mc:surface:"),
            "leaked surface comment into UI"
        );
    }

    #[test]
    fn render_expands_surface_overall_and_latest_ask_lines() {
        let doc = TrajectoryDoc::parse(
            "---
workspace: intent
---

## Mission
- Demo

## Current surfaces
- ✻ claude · claude · mbp · working   overall:Build detail view   ask:Show Beads rows              <!-- mc:surface:surface:11 -->

## Beads
- [ ] repo-1 active issue
",
        )
        .unwrap();
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 90, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    None,
                )
            })
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(dump.contains("overall:"), "missing overall line: {dump}");
        assert!(
            dump.contains("Build detail view"),
            "missing goal text: {dump}"
        );
        assert!(dump.contains("latest:"), "missing latest line: {dump}");
        assert!(
            dump.contains("Show Beads rows"),
            "missing latest ask: {dump}"
        );
    }

    #[test]
    fn render_with_no_doc_shows_placeholder() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, 80, 10), None, 0, false, None, None, None))
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
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 80, 24),
                    Some(&doc),
                    0,
                    true,
                    Some(&state),
                    None,
                    None,
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // The cursor cell (first item in Goal) should have a Cyan background.
        // Find the first row that contains "Build self-improvement" and check BG color.
        let mut found = false;
        'outer: for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().chars().next().unwrap_or(' '))
                })
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
            mode: EditMode::Insert {
                focus: InsertFocus::Item,
            },
            edit_buffer: "Edited goal".to_string(),
            ..Default::default()
        };
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 80, 30),
                    Some(&doc),
                    0,
                    true,
                    Some(&state),
                    None,
                    None,
                )
            })
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("input context"),
            "missing input context strip"
        );
        assert!(dump.contains("Edited goal"), "buffer text not shown");
    }

    #[test]
    fn render_highlights_empty_section_header_when_cursor_on_it() {
        use crate::mc_data::trajectory::TrajectoryDoc;
        let mut doc = TrajectoryDoc::default();
        doc.ensure_sections(); // all sections empty
        // Cursor on section 1 (Current surfaces), which is empty.
        let state = TrajectoryEditState {
            cursor_section: 1,
            cursor_item: 0,
            mode: EditMode::Nav,
            ..Default::default()
        };
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 80, 20),
                    Some(&doc),
                    0,
                    true,
                    Some(&state),
                    None,
                    None,
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // Find the row containing "Current surfaces" and verify Cyan background.
        let mut found = false;
        'outer: for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            if row.contains("Current surfaces") {
                for x in 0..buf.area.width {
                    if let Some(cell) = buf.cell((x, y)) {
                        if cell.symbol() == "C" {
                            assert_eq!(
                                cell.style().bg,
                                Some(Color::Cyan),
                                "empty-section header should have Cyan bg when cursor is on it"
                            );
                            found = true;
                            break 'outer;
                        }
                    }
                }
            }
        }
        assert!(found, "did not find highlighted empty-section header");
    }

    #[test]
    fn render_insert_mode_shows_edit_buffer_text() {
        let mut doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        doc.ensure_sections();
        let state = TrajectoryEditState {
            cursor_section: 2, // Tasks
            cursor_item: 1,    // sprint-02
            mode: EditMode::Insert {
                focus: InsertFocus::Item,
            },
            edit_buffer: "sprint-02 in progress".to_string(),
            ..Default::default()
        };
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 80, 30),
                    Some(&doc),
                    0,
                    true,
                    Some(&state),
                    None,
                    None,
                )
            })
            .unwrap();
        let dump = buf_dump(&terminal);
        assert!(
            dump.contains("sprint-02 in progress"),
            "edit buffer text not shown: {dump}"
        );
    }

    #[test]
    fn render_uses_workspace_accent_for_detail_border() {
        let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 80, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    Some("#C0392B"),
                )
            })
            .unwrap();

        let border_cell = terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .expect("top-left border cell should exist");
        assert_eq!(border_cell.style().fg, Some(Color::Rgb(0xC0, 0x39, 0x2B)));
    }

    // ── T3: surface glyphs + goal/surface badges ──────────────────────────────

    const T3_SAMPLE: &str = "---
workspace: t3
---

## Mission
- Demo

## Current surfaces
- ✻ claude · claude · mbp · working   ← goal:Wire up T3 rendering              <!-- mc:surface:surface:11 -->
- ▲ codex · shell · mbp · idle              <!-- mc:surface:surface:22 -->

## Beads
- [ ] Wire up T3 rendering   → ✻ surface:11
- [x] Land T0 rename
";

    /// Look up the foreground color of the first cell in the row whose text
    /// contains `needle`, where the cell symbol equals `marker`.
    fn cell_fg(terminal: &Terminal<TestBackend>, needle: &str, marker: char) -> Option<Color> {
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            if row.contains(needle) {
                for x in 0..buf.area.width {
                    if let Some(cell) = buf.cell((x, y)) {
                        if cell.symbol().chars().next() == Some(marker) {
                            return cell.style().fg;
                        }
                    }
                }
            }
        }
        None
    }

    fn cell_style(terminal: &Terminal<TestBackend>, needle: &str, marker: char) -> Option<Style> {
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            if row.contains(needle) {
                for x in 0..buf.area.width {
                    if let Some(cell) = buf.cell((x, y)) {
                        if cell.symbol().chars().next() == Some(marker) {
                            return Some(cell.style());
                        }
                    }
                }
            }
        }
        None
    }

    #[test]
    fn surface_glyph_gets_kind_color() {
        let doc = TrajectoryDoc::parse(T3_SAMPLE).unwrap();
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let hints = RenderHints::default();
        terminal
            .draw(|f| {
                render_with_hints(
                    f,
                    Rect::new(0, 0, 120, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    None,
                    &hints,
                )
            })
            .unwrap();

        // Claude glyph: orange.
        assert_eq!(
            cell_fg(&terminal, "claude · claude", '✻'),
            Some(kind_color(SurfaceKind::Claude)),
            "Claude glyph should be tinted with Claude's color"
        );
        // Codex glyph: cyan-blue.
        assert_eq!(
            cell_fg(&terminal, "codex · shell", '▲'),
            Some(kind_color(SurfaceKind::Codex)),
            "Codex glyph should be tinted with Codex's color"
        );
    }

    #[test]
    fn just_exited_surface_renders_with_dim_modifier() {
        let doc = TrajectoryDoc::parse(T3_SAMPLE).unwrap();
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        // The Codex surface (surface:22) is in the dim set: it represents a
        // surface whose current kind is Shell but effective_kind elevated it
        // to Codex via a fresh last-agent snapshot.
        let mut hints = RenderHints::default();
        hints.dim_surface_refs.insert("surface:22".to_string());

        terminal
            .draw(|f| {
                render_with_hints(
                    f,
                    Rect::new(0, 0, 120, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    None,
                    &hints,
                )
            })
            .unwrap();

        let style = cell_style(&terminal, "codex · shell", '▲')
            .expect("codex row should be present in the buffer");
        assert!(
            style.add_modifier.contains(Modifier::DIM),
            "just-exited glyph should carry Modifier::DIM, got {:?}",
            style
        );

        // Sanity: the live Claude surface (not in dim set) is NOT dim.
        let live =
            cell_style(&terminal, "claude · claude", '✻').expect("claude row should be present");
        assert!(
            !live.add_modifier.contains(Modifier::DIM),
            "live Claude glyph should not be DIM, got {:?}",
            live
        );
    }

    #[test]
    fn surface_row_renders_goal_badge_in_dark_gray() {
        let doc = TrajectoryDoc::parse(T3_SAMPLE).unwrap();
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let hints = RenderHints::default();
        terminal
            .draw(|f| {
                render_with_hints(
                    f,
                    Rect::new(0, 0, 120, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    None,
                    &hints,
                )
            })
            .unwrap();

        let dump: String = {
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
        };
        assert!(
            dump.contains("← goal:Wire up T3 rendering"),
            "missing surface goal badge in render: {dump}"
        );
        // Badge color is DarkGray on the `←` glyph.
        assert_eq!(
            cell_fg(&terminal, "← goal:", '←'),
            Some(Color::DarkGray),
            "← goal badge should render in DarkGray"
        );
    }

    #[test]
    fn goal_row_renders_surface_badge_in_dark_gray() {
        let doc = TrajectoryDoc::parse(T3_SAMPLE).unwrap();
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let hints = RenderHints::default();
        terminal
            .draw(|f| {
                render_with_hints(
                    f,
                    Rect::new(0, 0, 120, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    None,
                    &hints,
                )
            })
            .unwrap();
        assert_eq!(
            cell_fg(&terminal, "Wire up T3 rendering   →", '→'),
            Some(Color::DarkGray),
            "→ surface badge should render in DarkGray"
        );
    }

    #[test]
    fn no_goals_no_change_in_rendered_buffer() {
        // A workspace with no goals.json renders exactly as if T3 had never
        // been applied: no `← goal:` and no `→` badges in the buffer dump.
        let bare = "---
workspace: bare
---

## Mission
- Mission item

## Current surfaces

## Beads
- [ ] An ordinary goal
";
        let doc = TrajectoryDoc::parse(bare).unwrap();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let hints = RenderHints::default();
        terminal
            .draw(|f| {
                render_with_hints(
                    f,
                    Rect::new(0, 0, 80, 20),
                    Some(&doc),
                    0,
                    false,
                    None,
                    None,
                    None,
                    &hints,
                )
            })
            .unwrap();

        let dump: String = {
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
        };
        assert!(
            !dump.contains("← goal:"),
            "no surface badge expected: {dump}"
        );
        assert!(!dump.contains("   → "), "no goal badge expected: {dump}");
        assert!(dump.contains("An ordinary goal"), "goal text missing");
    }
}
