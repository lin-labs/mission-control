use crate::tui::app::WorkspaceState;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

pub fn render_sidebar(
    f: &mut Frame,
    area: Rect,
    workspaces: &[WorkspaceState],
    selected: usize,
    focused: bool,
) {
    // Inner width = area width minus 2 border columns.
    let inner_width = area.width.saturating_sub(2);
    let content_width = inner_width.saturating_sub(row_edge_width());

    let items: Vec<ListItem> = workspaces
        .iter()
        .enumerate()
        .map(|(idx, ws)| {
            let is_selected = idx == selected;
            let (badge_text, badge_color) = agent_badge(ws);
            // Spinner when an async refresh is in flight, otherwise status dot
            let (state_dot, state_color) = if ws.loading {
                (spinner_frame().to_string(), Color::Cyan)
            } else {
                let (dot, c) = status_indicator(ws);
                (dot.to_string(), c)
            };
            let host_badge = ws
                .session
                .as_ref()
                .and_then(|s| s.frontmatter.host.as_deref())
                .filter(|h| *h != "mbp")
                .map(|h| format!(" [{}]", h))
                .unwrap_or_default();

            let accent_color =
                crate::sidebar_pure::workspace_accent_color(ws.workspace.custom_color.as_deref());
            // "[XX] ● " = 4 (badge) + 1 (space) + 1 (dot) + 1 (space) = 7 cells, all single-width.
            let prefix_cols: u16 = 7;
            let display_name = truncate_for_width(
                &ws.workspace.name,
                content_width
                    .saturating_sub(prefix_cols)
                    .saturating_sub(host_badge.chars().count() as u16),
            );

            let name_line = decorate_sidebar_line(
                Line::from(vec![
                    Span::styled(
                        format!("{} ", badge_text),
                        Style::default().fg(badge_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{} ", state_dot), Style::default().fg(state_color)),
                    Span::styled(
                        display_name,
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(host_badge, Style::default().fg(Color::DarkGray)),
                ]),
                accent_color,
            );
            let item_style = if is_selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            // Optionally render a dim description subtitle line.
            if let Some(sub) =
                description_subtitle_line(ws.workspace.description.as_deref(), content_width)
            {
                ListItem::new(vec![name_line, decorate_sidebar_line(sub, accent_color)])
                    .style(item_style)
            } else {
                ListItem::new(name_line).style(item_style)
            }
        })
        .collect();

    let border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let list = List::new(items).block(
        Block::default()
            .title(" Workspaces ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );

    let mut state = ListState::default();
    state.select(Some(selected));

    f.render_stateful_widget(list, area, &mut state);
}

/// Build the dim subtitle `Line` for a workspace description, or `None` if there is nothing to
/// show.
///
/// Rules:
/// - Only the first line of `description` is used (split on `\n`).
/// - Leading/trailing whitespace is trimmed.
/// - The result is indented by 2 spaces.
/// - If the text (after indent) would exceed `sidebar_inner_width`, it is truncated with `…`.
/// - Returns `None` when `description` is `None` or blank after trimming.
///
/// The same function is exposed from the library crate at
/// `mission_control::sidebar_pure::description_subtitle_line` for integration tests.
pub fn description_subtitle_line(
    description: Option<&str>,
    sidebar_inner_width: u16,
) -> Option<Line<'static>> {
    let raw = description?;
    let first = raw.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }

    // Available columns: inner_width minus 2-space indent, minus 1 for the possible ellipsis.
    // We need at least 1 column for content.
    let indent = "  ";
    let indent_len = 2u16;
    let max_text_cols = sidebar_inner_width
        .saturating_sub(indent_len)
        .saturating_sub(1);
    if max_text_cols == 0 {
        return None;
    }

    // Count Unicode scalar values (chars) as a proxy for display columns (ASCII-safe).
    let char_count = first.chars().count();
    let text: String = if char_count > max_text_cols as usize {
        let truncated: String = first.chars().take(max_text_cols as usize).collect();
        format!("{}…", truncated)
    } else {
        first.to_owned()
    };

    let line = Line::from(vec![
        Span::raw(indent),
        Span::styled(text, Style::default().fg(Color::DarkGray)),
    ]);
    Some(line)
}

/// Per-agent badge shown at the start of each sidebar row.
/// Two-letter code in brand color, plus a fallback for non-agent shells.
fn agent_badge(ws: &WorkspaceState) -> (&'static str, Color) {
    match ws.agent_name() {
        "claude" => ("[CC]", Color::Rgb(0xCC, 0x78, 0x5C)),    // Anthropic orange
        "codex" => ("[CD]", Color::Rgb(0x10, 0xA3, 0x7C)),     // OpenAI green
        "opencode" => ("[OC]", Color::Rgb(0xA8, 0x7B, 0xCC)),  // purple
        _ => ("[SH]", Color::DarkGray),                        // shell / non-agent
    }
}

fn status_indicator(ws: &WorkspaceState) -> (&str, Color) {
    use crate::tui::app::AgentState;
    match ws.agent_state() {
        AgentState::Working => ("\u{25cf}", Color::Green), // ● baking
        AgentState::NeedsMe => ("\u{26a0}", Color::Yellow), // ⚠ needs you
        AgentState::Idle => ("\u{25cb}", Color::DarkGray), // ○ nothing happening
    }
}

/// Time-based braille spinner frame. Rotates every ~80ms as long as the UI redraws.
pub fn spinner_frame() -> char {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[((ms / 80) as usize) % FRAMES.len()]
}

fn row_edge_width() -> u16 {
    4
}

fn decorate_sidebar_line(line: Line<'static>, accent_color: Option<Color>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 2);

    if let Some(color) = accent_color {
        spans.push(Span::styled("▌ ", Style::default().fg(color)));
    } else {
        spans.push(Span::raw("  "));
    }

    spans.extend(line.spans);

    if let Some(color) = accent_color {
        spans.push(Span::styled(" ▐", Style::default().fg(color)));
    } else {
        spans.push(Span::raw("  "));
    }

    Line::from(spans)
}

fn truncate_for_width(text: &str, max_cols: u16) -> String {
    if max_cols == 0 {
        return String::new();
    }

    let char_count = text.chars().count();
    if char_count <= max_cols as usize {
        return text.to_owned();
    }

    if max_cols == 1 {
        return "…".to_string();
    }

    let truncated: String = text.chars().take(max_cols as usize - 1).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmux::client::Workspace;
    use crate::tui::app::{DismissalState, RegenSchedulerState, ScreenInsights, WorkspaceState};
    use ratatui::{Terminal, backend::TestBackend};

    fn test_workspace_state(name: &str, custom_color: Option<&str>) -> WorkspaceState {
        WorkspaceState {
            workspace: Workspace {
                ref_id: "workspace:1".to_string(),
                uuid: "workspace-1".to_string(),
                name: name.to_string(),
                selected: true,
                description: Some("short description".to_string()),
                current_directory: None,
                custom_color: custom_color.map(str::to_string),
            },
            session: None,
            surfaces: Vec::new(),
            screen_preview: None,
            screen_insights: ScreenInsights::default(),
            tool_call_count: 0,
            notes: None,
            hook_status: None,
            classification: None,
            loading: false,
            summary: None,
            summarizing: false,
            trajectory: None,
            edit_state: None,
            peek_state: None,
            peek_yield_pending: false,
            regen: RegenSchedulerState::default(),
            dismissal: DismissalState::default(),
            dispatch_modal: None,
            dispatch_pending_outcome: None,
            dispatch_error: None,
        }
    }

    #[test]
    fn render_sidebar_uses_side_accents_instead_of_name_tint() {
        let workspaces = vec![test_workspace_state("alpha", Some("#C0392B"))];
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 40, 6), &workspaces, 0, true))
            .unwrap();

        let buf = terminal.backend().buffer();
        let mut found_name = false;

        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();

            if row.contains("alpha") {
                let left_bar = (0..buf.area.width)
                    .find_map(|x| buf.cell((x, y)).filter(|cell| cell.symbol() == "▌"))
                    .expect("left accent bar should be present");
                assert_eq!(left_bar.style().fg, Some(Color::Rgb(0xC0, 0x39, 0x2B)));

                for x in 0..buf.area.width {
                    if let Some(cell) = buf.cell((x, y)) {
                        if cell.symbol() == "a" {
                            assert_eq!(cell.style().fg, Some(Color::White));
                            assert_eq!(cell.style().bg, Some(Color::DarkGray));
                            found_name = true;
                            break;
                        }
                    }
                }
            }
        }

        assert!(found_name, "workspace name was not rendered");
    }
}
