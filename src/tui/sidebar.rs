use crate::tui::app::WorkspaceState;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};

/// Width consumed by the heavy-box edges on each workspace row.
/// Layout: `┃ <content> ┃` — 1 box char + 1 pad cell on each side = 4 cells of chrome.
const BOX_EDGE_COLS: u16 = 4;

pub fn render_sidebar(
    f: &mut Frame,
    area: Rect,
    workspaces: &[WorkspaceState],
    selected: usize,
    focused: bool,
) {
    // Inner width = area width minus 2 outer-border columns.
    let inner_width = area.width.saturating_sub(2);
    // Between-the-┃s width (the bar above/below + the content cells).
    let bar_width = inner_width.saturating_sub(2);
    // Width available for the actual title text inside the padded box.
    let text_width = inner_width.saturating_sub(BOX_EDGE_COLS);

    let bar = "━".repeat(bar_width as usize);

    let items: Vec<ListItem> = workspaces
        .iter()
        .enumerate()
        .map(|(idx, ws)| {
            let is_selected = idx == selected;
            let is_last = idx + 1 == workspaces.len();

            // Status leader: spinner when refresh in flight, otherwise dot.
            let (leader, leader_color) = if ws.loading {
                (spinner_frame().to_string(), Color::Cyan)
            } else {
                let (dot, c) = status_indicator(ws);
                (dot.to_string(), c)
            };
            let leader_str = format!("{} ", leader);

            let host_badge = ws
                .session
                .as_ref()
                .and_then(|s| s.frontmatter.host.as_deref())
                .filter(|h| *h != "mbp")
                .map(|h| format!(" [{}]", h))
                .unwrap_or_default();

            let accent_color = crate::sidebar_pure::workspace_accent_color(
                ws.workspace.custom_color.as_deref(),
            )
            .unwrap_or(Color::DarkGray);

            // Truncate name to fit between leader and host_badge inside the box.
            let name_max = (text_width as usize)
                .saturating_sub(leader_str.chars().count())
                .saturating_sub(host_badge.chars().count());
            let display_name = truncate_for_width(&ws.workspace.name, name_max as u16);

            let used = 1
                + leader_str.chars().count()
                + display_name.chars().count()
                + host_badge.chars().count();
            let pad = " ".repeat((bar_width as usize).saturating_sub(used));

            // Box borders carry the workspace's accent color so the panel reads
            // as a unified colored frame. Interior cells fill with the accent
            // as background to give the box a colored body, not just sides.
            let border_style = Style::default().fg(accent_color);
            let interior_bg = accent_color;
            // Selection inverts the interior so the workspace ID color still
            // shows but the row clearly punches forward.
            let select_mod = if is_selected {
                Modifier::REVERSED
            } else {
                Modifier::empty()
            };

            let top_line = Line::from(Span::styled(format!("┏{}┓", bar), border_style));
            let bot_line = Line::from(Span::styled(format!("┗{}┛", bar), border_style));

            let content_line = Line::from(vec![
                Span::styled("┃", border_style),
                Span::styled(" ", Style::default().bg(interior_bg).add_modifier(select_mod)),
                Span::styled(
                    leader_str,
                    Style::default()
                        .fg(leader_color)
                        .bg(interior_bg)
                        .add_modifier(select_mod),
                ),
                Span::styled(
                    display_name,
                    Style::default()
                        .fg(Color::White)
                        .bg(interior_bg)
                        .add_modifier(Modifier::BOLD | select_mod),
                ),
                Span::styled(
                    host_badge,
                    Style::default()
                        .fg(Color::Gray)
                        .bg(interior_bg)
                        .add_modifier(select_mod),
                ),
                Span::styled(
                    pad,
                    Style::default().bg(interior_bg).add_modifier(select_mod),
                ),
                Span::styled("┃", border_style),
            ]);

            // 3-line box + 1 trailing blank gap (except after the last
            // workspace). Putting the gap inside the ListItem keeps
            // ListState.select(idx) lined up with the workspace's index.
            let mut lines = vec![top_line, content_line, bot_line];
            if !is_last {
                lines.push(Line::raw(""));
            }
            ListItem::new(lines)
        })
        .collect();

    let outer_border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Workspaces ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(outer_border_color)),
        )
        // Selection styling is applied per-span above (Modifier::REVERSED on
        // the interior) so the box border keeps its accent color. Disable
        // the List's default highlight so it doesn't paint over our spans.
        .highlight_style(Style::default());

    let mut state = ListState::default();
    if !workspaces.is_empty() {
        state.select(Some(selected));
    }

    f.render_stateful_widget(list, area, &mut state);
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

    fn buf_row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .filter_map(|x| {
                buf.cell((x, y))
                    .map(|c| c.symbol().chars().next().unwrap_or(' '))
            })
            .collect()
    }

    #[test]
    fn workspace_content_row_has_accent_bg_fill() {
        let workspaces = vec![test_workspace_state("alpha", Some("#C0392B"))];
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 8), &workspaces, 0, true))
            .unwrap();

        let alpha_y = (0..8)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");

        let buf = terminal.backend().buffer();

        // The 'a' cell should sit on the workspace's accent bg.
        let name_a = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y)).filter(|cell| cell.symbol() == "a"))
            .expect("alpha name should be rendered");
        assert_eq!(
            name_a.style().bg,
            Some(Color::Rgb(0xC0, 0x39, 0x2B)),
            "interior bg should be the workspace's accent color"
        );

        // The ┃ vertical border on the same row also stays in the accent
        // color (fg) but with no bg fill, so the box outline reads clean.
        let left_pipe = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y)).filter(|cell| cell.symbol() == "┃"))
            .expect("left ┃ should be present");
        assert_eq!(left_pipe.style().fg, Some(Color::Rgb(0xC0, 0x39, 0x2B)));
    }

    #[test]
    fn adjacent_workspaces_have_one_line_gap() {
        let workspaces = vec![
            test_workspace_state("alpha", Some("#C0392B")),
            test_workspace_state("beta", Some("#006B6B")),
        ];
        let backend = TestBackend::new(30, 14);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 14), &workspaces, 0, true))
            .unwrap();

        let alpha_y = (0..14)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");
        let beta_y = (0..14)
            .find(|y| buf_row(&terminal, *y).contains("beta"))
            .expect("beta should render");

        // 3 lines per box + 1 blank gap = 4 rows between content rows.
        assert_eq!(
            beta_y - alpha_y,
            4,
            "expected 4 rows between adjacent workspace content rows (3-line box + 1 gap), got {}",
            beta_y - alpha_y
        );

        // The gap row between alpha's bottom border and beta's top border
        // should be empty (no box characters).
        let gap_y = alpha_y + 2; // alpha content -> alpha bottom -> gap
        let gap_row = buf_row(&terminal, gap_y);
        assert!(
            !gap_row.contains('┏')
                && !gap_row.contains('┗')
                && !gap_row.contains('┓')
                && !gap_row.contains('┛'),
            "gap row should not contain box characters, got: {:?}",
            gap_row
        );
    }

    #[test]
    fn selected_workspace_inverts_interior() {
        let workspaces = vec![
            test_workspace_state("alpha", Some("#C0392B")),
            test_workspace_state("beta", Some("#006B6B")),
        ];
        let backend = TestBackend::new(30, 14);
        let mut terminal = Terminal::new(backend).unwrap();

        // Select beta (idx 1).
        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 14), &workspaces, 1, true))
            .unwrap();

        let beta_y = (0..14)
            .find(|y| buf_row(&terminal, *y).contains("beta"))
            .expect("beta should render");

        let buf = terminal.backend().buffer();

        // The selected workspace's name cell carries Modifier::REVERSED.
        let beta_b = (0..buf.area.width)
            .find_map(|x| buf.cell((x, beta_y)).filter(|cell| cell.symbol() == "b"))
            .expect("beta name should be rendered");
        assert!(
            beta_b.style().add_modifier.contains(Modifier::REVERSED),
            "selected workspace content should carry Modifier::REVERSED"
        );

        // The vertical border on the same row should NOT be reversed —
        // the box outline reads stable regardless of selection.
        let beta_pipe = (0..buf.area.width)
            .find_map(|x| buf.cell((x, beta_y)).filter(|cell| cell.symbol() == "┃"))
            .expect("beta ┃ should be present");
        assert!(
            !beta_pipe.style().add_modifier.contains(Modifier::REVERSED),
            "vertical border should not be reversed"
        );

        // Unselected (alpha) should not be reversed either.
        let alpha_y = (0..14)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");
        let alpha_a = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y)).filter(|cell| cell.symbol() == "a"))
            .expect("alpha name should be rendered");
        assert!(
            !alpha_a.style().add_modifier.contains(Modifier::REVERSED),
            "unselected workspace should not be reversed"
        );
    }

    #[test]
    fn workspace_without_custom_color_uses_dark_gray_accent() {
        let workspaces = vec![test_workspace_state("plain", None)];
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 6), &workspaces, 0, true))
            .unwrap();

        let y = (0..6)
            .find(|y| buf_row(&terminal, *y).contains("plain"))
            .expect("plain should render");
        let buf = terminal.backend().buffer();

        let name_p = (0..buf.area.width)
            .find_map(|x| buf.cell((x, y)).filter(|cell| cell.symbol() == "p"))
            .expect("plain name should be rendered");
        assert_eq!(name_p.style().bg, Some(Color::DarkGray));
    }
}
