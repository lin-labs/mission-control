use std::collections::HashSet;

use crate::mc_data::surface_kind::SurfaceKind;
use crate::mc_data::trajectory::{
    Item, SECTION_CURRENT_SURFACES, SECTION_GOALS, Section, TrajectoryDoc,
};
use crate::tui::peek_view::PeekState;
use crate::tui::trajectory_edit::{EditMode, InsertFocus, MissionFocus, TrajectoryEditState};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

const ACTIVE_MISSION_MAX_CHARS: usize = 110;

/// Map a kind to the text color used for its glyph + label in surface rows.
pub fn kind_color(kind: SurfaceKind) -> Color {
    match kind {
        SurfaceKind::Claude => Color::Rgb(217, 119, 6),
        SurfaceKind::Codex => Color::Rgb(6, 182, 212),
        SurfaceKind::OtherAgent => Color::Magenta,
        SurfaceKind::Shell => Color::Gray,
        SurfaceKind::Remote => Color::Rgb(34, 197, 94),
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
        '⇅' => Some(SurfaceKind::Remote),
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

/// Max wrapped lines a single overall/latest field may occupy. Beyond this the
/// text is truncated with `…` rather than pushing the surface block taller. A
/// short value still renders on one line; a medium one wraps to 2–4.
const MAX_FIELD_LINES: usize = 4;

fn surface_item_lines<'a>(
    prefix: &str,
    text: &'a str,
    dim: bool,
    base: Style,
    cursor: bool,
    inner_width: usize,
) -> Vec<Line<'a>> {
    let (main, overall, ask) = split_surface_intent(text);
    // The main (title) line carries the nav cursor highlight when selected; the
    // overall/latest sub-lines render identically whether or not the row is the
    // cursor, so selecting a surface doesn't change its structure.
    let first_line = if cursor {
        Line::from(Span::styled(
            format!("{prefix}{main}"),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ))
    } else {
        let mut first = vec![Span::styled(prefix.to_string(), base)];
        first.extend(surface_row_spans(main, dim, base));
        Line::from(first)
    };
    let mut lines = vec![first_line];
    if let Some(goal) = overall.filter(|s| !s.is_empty()) {
        push_field_lines(&mut lines, "    overall: ", goal, base, inner_width);
    }
    if let Some(ask) = ask.filter(|s| !s.is_empty()) {
        push_field_lines(&mut lines, "    latest:  ", ask, base, inner_width);
    }
    lines
}

/// Push an `overall:`/`latest:` field as 1–`MAX_FIELD_LINES` wrapped lines:
/// the label leads the first line, continuation lines are indented to align
/// under the value. Pre-wrapping to the available width (rather than leaning on
/// the Paragraph's own wrap) lets us cap the field at `MAX_FIELD_LINES`.
fn push_field_lines<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &'static str,
    value: &str,
    base: Style,
    inner_width: usize,
) {
    let indent = label.chars().count();
    // Leave room for the label on the first line and matching indent on the
    // rest. Floor the wrap width so a very narrow pane still makes progress.
    let avail = inner_width.saturating_sub(indent).max(8);
    let wrapped = wrap_words(value, avail, MAX_FIELD_LINES);
    let pad: String = " ".repeat(indent);
    for (i, segment) in wrapped.into_iter().enumerate() {
        let lead = if i == 0 {
            Span::styled(label, base.fg(Color::DarkGray))
        } else {
            Span::styled(pad.clone(), base)
        };
        lines.push(Line::from(vec![
            lead,
            Span::styled(segment, base.fg(Color::Gray)),
        ]));
    }
}

/// Greedy word-wrap `text` to `width` columns, capped at `max_lines`. Words
/// longer than `width` are hard-split. If the text doesn't fit in `max_lines`,
/// the last line ends with `…`.
fn wrap_words(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;

    // Emit `cur` and reset; returns false once we've hit the line budget.
    let mut overflow = false;
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        if cur_len == 0 {
            // Hard-split a word that's wider than the whole line.
            if wlen > width {
                let mut chars = word.chars().peekable();
                while chars.peek().is_some() && lines.len() < max_lines {
                    let chunk: String = chars.by_ref().take(width).collect();
                    lines.push(chunk);
                }
                // Leftover characters past the line budget → mark overflow so
                // the ellipsis pass trims the final line.
                if chars.peek().is_some() {
                    overflow = true;
                }
                continue;
            }
            cur.push_str(word);
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut cur));
            if lines.len() >= max_lines {
                overflow = true;
                break;
            }
            cur.push_str(word);
            cur_len = wlen;
        }
    }
    if !overflow && !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_lines {
        overflow = true;
        lines.truncate(max_lines);
    }
    if overflow {
        // Append an ellipsis to the last kept line, trimming to fit width.
        if let Some(last) = lines.last_mut() {
            let mut chars: Vec<char> = last.chars().collect();
            while chars.len() + 1 > width && !chars.is_empty() {
                chars.pop();
            }
            *last = chars.into_iter().collect();
            last.push('…');
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
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

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

fn mission_display_text(text: &str) -> String {
    truncate_chars(text.trim(), ACTIVE_MISSION_MAX_CHARS)
}

/// Per-section, per-item context for dim-glyph decisions. Empty by default.
#[derive(Default, Debug, Clone)]
pub struct RenderHints {
    /// Set of surface refs whose glyph should be rendered with Modifier::DIM
    /// (current kind is Shell/Unknown but effective_kind elevated it from a
    /// fresh last-agent snapshot).
    pub dim_surface_refs: HashSet<String>,
    /// Display-only override for the canonical third trajectory section.
    /// Persisted trajectory files keep `## Beads` for backward compatibility.
    pub task_section_title: Option<String>,
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
    // Line index (into `lines`) where the nav cursor's row begins. Used below
    // to scroll the viewport so the cursor stays visible as `j`/`k` move it.
    let mut cursor_line: Option<u16> = None;
    for (sec_idx, section) in doc.sections.iter().enumerate() {
        if section.name == crate::mc_data::trajectory::SECTION_MISSION {
            if let Some(line) = render_mission_section(
                &mut lines,
                section,
                &doc.mission_history,
                sec_idx,
                edit_state,
            ) {
                cursor_line = Some(line);
            }
            lines.push(Line::raw(""));
            continue;
        }

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

        let section_title = if section.name == SECTION_GOALS {
            hints
                .task_section_title
                .as_deref()
                .unwrap_or(section.name.as_str())
        } else {
            section.name.as_str()
        };
        let header_line = if is_header_cursor {
            Line::from(Span::styled(
                format!("## {section_title}"),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(Span::styled(
                format!("## {section_title}"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ))
        };
        if is_header_cursor {
            cursor_line = Some(lines.len() as u16);
        }
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
                if is_cursor {
                    // Record where this item's first rendered line lands so the
                    // viewport can scroll to keep the cursor visible.
                    cursor_line = Some(lines.len() as u16);
                }

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
                // Current surfaces render as a clean multi-line block (title +
                // overall/latest) in nav mode — whether or not the row is the
                // cursor. Only the editor's insert mode (blocked here anyway)
                // falls through to the single-line path below.
                if section.name == SECTION_CURRENT_SURFACES && !is_insert_cursor {
                    let dim = item
                        .surface_id
                        .as_deref()
                        .map(|sid| hints.dim_surface_refs.contains(sid))
                        .unwrap_or(false);
                    let base = Style::default().fg(text_color);
                    lines.extend(surface_item_lines(
                        prefix,
                        display_text,
                        dim,
                        base,
                        is_cursor && !in_insert,
                        body_inner.width as usize,
                    ));
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

    // In nav mode the viewport follows the cursor: derive the scroll offset so
    // the cursor row is always on screen (it walks off-screen otherwise on a
    // long trajectory). Outside nav mode, honour the caller's manual scroll.
    let in_nav = edit_state
        .map(|s| matches!(s.mode, EditMode::Nav))
        .unwrap_or(false);
    let effective_scroll = match cursor_line {
        Some(cl) if in_nav => {
            let view_h = body_inner.height.max(1);
            if cl < scroll {
                cl
            } else if cl >= scroll + view_h {
                cl.saturating_sub(view_h - 1)
            } else {
                scroll
            }
        }
        _ => scroll,
    };

    let para = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((effective_scroll, 0));
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

fn mission_insert_line<'a>(
    prefix: &str,
    display_text: &str,
    edit_state: &TrajectoryEditState,
) -> Line<'a> {
    let cursor_col = edit_state.cursor_col;
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
}

fn mission_item_line<'a>(
    prefix: &str,
    text: &str,
    nav_cursor: bool,
    insert_state: Option<&TrajectoryEditState>,
) -> Line<'a> {
    if let Some(state) = insert_state {
        return mission_insert_line(prefix, state.edit_buffer.as_str(), state);
    }

    let display = mission_display_text(text);
    if nav_cursor {
        Line::from(Span::styled(
            format!("{prefix}{display}"),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ))
    } else {
        Line::from(Span::styled(
            format!("{prefix}{display}"),
            Style::default().fg(Color::Gray),
        ))
    }
}

fn render_mission_section<'a>(
    lines: &mut Vec<Line<'a>>,
    section: &'a Section,
    history: &'a [Item],
    sec_idx: usize,
    edit_state: Option<&TrajectoryEditState>,
) -> Option<u16> {
    let mut cursor_line = None;
    let header_cursor = edit_state
        .map(|s| {
            s.cursor_section == sec_idx
                && s.mission_focus == MissionFocus::Active
                && s.cursor_item == 0
                && section.items.is_empty()
                && matches!(s.mode, EditMode::Nav)
        })
        .unwrap_or(false);

    lines.push(Line::from(Span::styled(
        format!("## {}", section.name),
        if header_cursor {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        },
    )));
    if header_cursor {
        cursor_line = Some((lines.len() - 1) as u16);
    }

    if section.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (empty)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (item_idx, item) in section.items.iter().enumerate() {
            let active_cursor = edit_state
                .map(|s| {
                    s.cursor_section == sec_idx
                        && s.mission_focus == MissionFocus::Active
                        && s.cursor_item == item_idx
                        && matches!(s.mode, EditMode::Nav)
                })
                .unwrap_or(false);
            let active_insert = edit_state.filter(|s| {
                s.cursor_section == sec_idx
                    && s.mission_focus == MissionFocus::Active
                    && s.cursor_item == item_idx
                    && matches!(s.mode, EditMode::Insert { .. })
            });
            if active_cursor || active_insert.is_some() {
                cursor_line = Some(lines.len() as u16);
            }
            lines.push(mission_item_line(
                "- [ ] ",
                &item.text,
                active_cursor,
                active_insert,
            ));
        }
    }

    lines.push(Line::raw(""));
    let expanded = edit_state.is_some_and(|state| state.mission_history_expanded);
    let history_header_cursor = edit_state.is_some_and(|state| {
        state.cursor_section == sec_idx
            && state.mission_focus == MissionFocus::HistoryHeader
            && matches!(state.mode, EditMode::Nav)
    });
    let history_title = if history.is_empty() {
        "## Mission history (0)".to_string()
    } else if expanded {
        format!("## Mission history ({}) ▾ Enter to fold", history.len())
    } else {
        format!("## Mission history ({}) ▸ Enter to unfold", history.len())
    };
    let history_style = if history_header_cursor {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    if history_header_cursor {
        cursor_line = Some(lines.len() as u16);
    }
    lines.push(Line::from(Span::styled(history_title, history_style)));

    if expanded {
        for (history_idx, item) in history.iter().enumerate() {
            let cursor = edit_state.is_some_and(|state| {
                state.cursor_section == sec_idx
                    && state.mission_focus == MissionFocus::HistoryItem(history_idx)
                    && matches!(state.mode, EditMode::Nav)
            });
            if cursor {
                cursor_line = Some(lines.len() as u16);
            }
            let mut line = mission_item_line("- [x] ", &item.text, cursor, None);
            if !cursor {
                for span in &mut line.spans {
                    span.style = span.style.fg(Color::DarkGray);
                }
            }
            lines.push(line);
        }
    }

    cursor_line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::trajectory_edit::{EditMode, InsertFocus, TrajectoryEditState};
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

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn mission_section_renders_multiple_active_checkboxes_and_folded_history() {
        let section = Section {
            name: crate::mc_data::trajectory::SECTION_MISSION.to_string(),
            items: vec![
                crate::mc_data::trajectory::Item {
                    text: "Agent mission".to_string(),
                    is_checkbox: true,
                    checked: Some(false),
                    surface_id: None,
                },
                crate::mc_data::trajectory::Item {
                    text: "[h] Human mission".to_string(),
                    is_checkbox: true,
                    checked: Some(false),
                    surface_id: None,
                },
            ],
        };
        let history = vec![crate::mc_data::trajectory::Item {
            text: "Finished mission".to_string(),
            is_checkbox: true,
            checked: Some(true),
            surface_id: None,
        }];
        let mut lines = Vec::new();

        let _ = render_mission_section(&mut lines, &section, &history, 0, None);
        let rendered: Vec<String> = lines.iter().map(line_text).collect();

        assert_eq!(rendered[0], "## Mission");
        assert_eq!(rendered[1], "- [ ] Agent mission");
        assert_eq!(rendered[2], "- [ ] [h] Human mission");
        assert!(
            rendered
                .iter()
                .any(|line| { line == "## Mission history (1) ▸ Enter to unfold" })
        );
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("Finished mission"))
        );
    }

    #[test]
    fn expanded_mission_history_renders_every_completed_row() {
        let section = Section {
            name: crate::mc_data::trajectory::SECTION_MISSION.to_string(),
            items: vec![crate::mc_data::trajectory::Item {
                text: "Active mission".to_string(),
                is_checkbox: true,
                checked: Some(false),
                surface_id: None,
            }],
        };
        let mut history = Vec::new();
        for idx in 1..=15 {
            history.push(crate::mc_data::trajectory::Item {
                text: format!("Finished mission {idx:02}"),
                is_checkbox: true,
                checked: Some(true),
                surface_id: None,
            });
        }
        let edit_state = TrajectoryEditState {
            mission_history_expanded: true,
            mission_focus: crate::tui::trajectory_edit::MissionFocus::HistoryItem(14),
            ..TrajectoryEditState::default()
        };
        let mut lines = Vec::new();

        let _ = render_mission_section(&mut lines, &section, &history, 0, Some(&edit_state));
        let rendered: Vec<String> = lines.iter().map(line_text).collect();

        assert!(
            rendered
                .iter()
                .any(|line| { line == "## Mission history (15) ▾ Enter to fold" })
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.starts_with("- [x] Finished mission"))
                .count(),
            15
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Finished mission 15"))
        );
    }

    #[test]
    fn provisional_human_mission_renders_as_unchecked_insert_row() {
        let section = Section {
            name: crate::mc_data::trajectory::SECTION_MISSION.to_string(),
            items: vec![crate::mc_data::trajectory::Item {
                text: String::new(),
                is_checkbox: true,
                checked: Some(false),
                surface_id: None,
            }],
        };
        let edit_state = TrajectoryEditState {
            mode: EditMode::Insert {
                focus: InsertFocus::Item,
            },
            edit_buffer: "New human mission".to_string(),
            provisional_human_mission: true,
            cursor_col: 6,
            ..TrajectoryEditState::default()
        };
        let mut lines = Vec::new();

        let _ = render_mission_section(&mut lines, &section, &[], 0, Some(&edit_state));
        let rendered: Vec<String> = lines.iter().map(line_text).collect();

        assert!(rendered[1].starts_with("- [ ] New h"));
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
    fn render_can_label_canonical_task_section_as_linear() {
        let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let hints = RenderHints {
            task_section_title: Some("Linear".to_string()),
            ..RenderHints::default()
        };
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

        let dump = buf_dump(&terminal);
        assert!(dump.contains("Linear"), "missing Linear header: {dump}");
        assert!(!dump.contains("Beads"), "canonical header leaked: {dump}");
        assert!(
            dump.contains("sprint-01 done"),
            "task rows disappeared: {dump}"
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
    fn cursor_on_surface_row_keeps_overall_and_latest_structure() {
        // Regression: selecting a surface used to collapse it to a single raw
        // line (exposing inline `overall:`/`ask:` markers and shifting the
        // layout). The cursor row must keep the same multi-line structure,
        // just with the main line highlighted.
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
        let surfaces_idx = doc
            .sections
            .iter()
            .position(|s| s.name == SECTION_CURRENT_SURFACES)
            .expect("current surfaces section");
        let state = TrajectoryEditState {
            cursor_section: surfaces_idx,
            cursor_item: 0,
            mode: EditMode::Nav,
            ..Default::default()
        };
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 90, 20),
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
        // Structure preserved even though the row is selected.
        assert!(
            dump.contains("overall:") && dump.contains("Build detail view"),
            "selected surface dropped its overall line: {dump}"
        );
        assert!(
            dump.contains("latest:") && dump.contains("Show Beads rows"),
            "selected surface dropped its latest line: {dump}"
        );

        // The main (title) line carries the Cyan cursor highlight, and the raw
        // inline `overall:`/`ask:` markers are NOT on that highlighted line.
        let buf = terminal.backend().buffer();
        let mut highlighted_main = false;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            let is_main_title =
                row.contains("claude") && !row.contains("overall:") && !row.contains("latest:");
            if is_main_title
                && (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)))
                    .any(|c| c.style().bg == Some(Color::Cyan))
            {
                highlighted_main = true;
            }
        }
        assert!(
            highlighted_main,
            "selected surface's main line should have a Cyan highlight: {dump}"
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

    #[test]
    fn wrap_words_short_value_stays_one_line() {
        let out = wrap_words("build the detail view", 40, 4);
        assert_eq!(out, vec!["build the detail view".to_string()]);
    }

    #[test]
    fn wrap_words_wraps_to_multiple_lines() {
        // 5 words of width 5 at width 11 → "aaaaa bbbbb" per line.
        let out = wrap_words("aaaaa bbbbb ccccc ddddd", 11, 4);
        assert_eq!(
            out,
            vec!["aaaaa bbbbb".to_string(), "ccccc ddddd".to_string()]
        );
    }

    #[test]
    fn wrap_words_caps_at_max_lines_with_ellipsis() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let out = wrap_words(text, 9, 4);
        assert_eq!(out.len(), 4, "must not exceed MAX_FIELD_LINES");
        assert!(
            out.last().unwrap().ends_with('…'),
            "truncation marker: {out:?}"
        );
        // Every line fits the width budget (ellipsis included).
        assert!(
            out.iter().all(|l| l.chars().count() <= 9),
            "overflow: {out:?}"
        );
    }

    #[test]
    fn wrap_words_hard_splits_overlong_word() {
        let out = wrap_words("abcdefghijklmnop", 5, 4);
        assert_eq!(out, vec!["abcde", "fghij", "klmno", "p"]);
    }

    #[test]
    fn nav_cursor_scrolls_viewport_into_view() {
        // A Beads section longer than the viewport; the cursor on a late row
        // must be visible (viewport follows the cursor), and the first row
        // must have scrolled off-screen. Mission history is intentionally
        // capped, so it is not a valid unbounded-scroll fixture.
        let mut md = String::from(
            "---\n{}\n---\n\n## Mission\n- active mission\n\n## Current surfaces\n\n## Beads\n",
        );
        for i in 0..30 {
            md.push_str(&format!("- [ ] bead item number {i:02}\n"));
        }
        let doc = TrajectoryDoc::parse(&md).unwrap();

        let mut state = TrajectoryEditState::default();
        state.cursor_section = 2; // Beads
        state.cursor_item = 27; // a late row, well past one screenful

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(
                    f,
                    Rect::new(0, 0, 60, 12),
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
            dump.contains("bead item number 27"),
            "cursor row should be visible: {dump}"
        );
        assert!(
            !dump.contains("bead item number 00"),
            "viewport should have scrolled past the first row: {dump}"
        );
    }
}
