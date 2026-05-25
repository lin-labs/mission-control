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
    let neutral_style = Style::default().fg(Color::DarkGray);

    // Shared-edge layout: one top cap, then `content + separator` for each
    // workspace. The last workspace's separator IS the bottom cap, so adjacent
    // workspaces share their horizontal border instead of stacking two of them.
    let mut items: Vec<ListItem> = Vec::with_capacity(workspaces.len() * 2 + 1);

    if !workspaces.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("┏{}┓", bar),
            neutral_style,
        ))));
    }

    for (idx, ws) in workspaces.iter().enumerate() {
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

        let used =
            1 + leader_str.chars().count() + display_name.chars().count() + host_badge.chars().count();
        let pad = " ".repeat((bar_width as usize).saturating_sub(used));

        let accent_style = Style::default().fg(accent_color);
        let content_bg = if is_selected {
            Color::DarkGray
        } else {
            Color::Reset
        };

        let content_line = Line::from(vec![
            Span::styled("┃", accent_style),
            Span::styled(" ", Style::default().bg(content_bg)),
            Span::styled(
                leader_str,
                Style::default().fg(leader_color).bg(content_bg),
            ),
            Span::styled(
                display_name,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
                    .bg(content_bg),
            ),
            Span::styled(
                host_badge,
                Style::default().fg(Color::DarkGray).bg(content_bg),
            ),
            Span::styled(pad, Style::default().bg(content_bg)),
            Span::styled("┃", accent_style),
        ]);
        items.push(ListItem::new(content_line));

        // Separator below: T-intersection between workspaces, bottom cap on last.
        let separator_text = if is_last {
            format!("┗{}┛", bar)
        } else {
            format!("┣{}┫", bar)
        };
        items.push(ListItem::new(Line::from(Span::styled(
            separator_text,
            neutral_style,
        ))));
    }

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
        // Selection styling is applied per-span on the content row so the
        // ┃ borders keep their accent color. Disable the List's default
        // highlight so it doesn't paint over our per-span styles.
        .highlight_style(Style::default());

    let mut state = ListState::default();
    if !workspaces.is_empty() {
        // Content rows live at indices 1, 3, 5, … (offset by the top cap and
        // separators). Map the user-facing `selected` workspace index onto the
        // raw ListItem index so scroll still keeps the right row visible.
        state.select(Some(1 + selected * 2));
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
    fn adjacent_workspaces_share_a_t_intersection_separator() {
        let workspaces = vec![
            test_workspace_state("alpha", Some("#C0392B")),
            test_workspace_state("beta", Some("#006B6B")),
        ];
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 12), &workspaces, 0, true))
            .unwrap();

        let alpha_y = (0..12)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");
        let beta_y = (0..12)
            .find(|y| buf_row(&terminal, *y).contains("beta"))
            .expect("beta should render");

        // Between alpha and beta there should be exactly ONE line, and that
        // line should contain a T-intersection separator ┣━━━┫ — not two
        // stacked borders.
        assert_eq!(
            beta_y - alpha_y,
            2,
            "beta should be exactly 2 rows below alpha (1 content + 1 shared separator), got {}",
            beta_y - alpha_y
        );
        let between = buf_row(&terminal, alpha_y + 1);
        assert!(
            between.contains('┣') && between.contains('┫'),
            "expected ┣ … ┫ shared separator between workspaces, got: {:?}",
            between
        );

        // Above alpha should be the top cap ┏━━━┓.
        let above = buf_row(&terminal, alpha_y - 1);
        assert!(
            above.contains('┏') && above.contains('┓'),
            "expected ┏ … ┓ top cap above first workspace, got: {:?}",
            above
        );

        // Below beta should be the bottom cap ┗━━━┛.
        let below = buf_row(&terminal, beta_y + 1);
        assert!(
            below.contains('┗') && below.contains('┛'),
            "expected ┗ … ┛ bottom cap below last workspace, got: {:?}",
            below
        );
    }

    #[test]
    fn content_row_uses_accent_color_on_vertical_borders() {
        let workspaces = vec![test_workspace_state("alpha", Some("#C0392B"))];
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 6), &workspaces, 0, true))
            .unwrap();

        let alpha_y = (0..6)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");

        let buf = terminal.backend().buffer();
        let left_pipe = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y)).filter(|cell| cell.symbol() == "┃"))
            .expect("left ┃ on content row should be present");
        assert_eq!(left_pipe.style().fg, Some(Color::Rgb(0xC0, 0x39, 0x2B)));

        // Horizontal separators stay neutral so adjacent workspaces don't
        // fight for the same shared edge color.
        let above = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y - 1)).filter(|cell| cell.symbol() == "┏"))
            .expect("top cap corner should be present");
        assert_eq!(above.style().fg, Some(Color::DarkGray));
    }

    #[test]
    fn selected_workspace_fills_content_with_dark_gray_bg() {
        let workspaces = vec![
            test_workspace_state("alpha", Some("#C0392B")),
            test_workspace_state("beta", Some("#006B6B")),
        ];
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 12), &workspaces, 1, true))
            .unwrap();

        let beta_y = (0..12)
            .find(|y| buf_row(&terminal, *y).contains("beta"))
            .expect("beta should render");
        let buf = terminal.backend().buffer();

        let beta_b = (0..buf.area.width)
            .find_map(|x| buf.cell((x, beta_y)).filter(|cell| cell.symbol() == "b"))
            .expect("beta name should be rendered");
        assert_eq!(
            beta_b.style().bg,
            Some(Color::DarkGray),
            "selected row interior should have DarkGray bg"
        );

        // The accent ┃ on the same content row should NOT receive bg fill —
        // the box outline stays stable regardless of selection.
        let beta_pipe = (0..buf.area.width)
            .find_map(|x| buf.cell((x, beta_y)).filter(|cell| cell.symbol() == "┃"))
            .expect("beta box ┃ should be present");
        assert_ne!(
            beta_pipe.style().bg,
            Some(Color::DarkGray),
            "vertical border should not receive selection bg"
        );

        // The unselected workspace (alpha) should NOT have bg fill on its name.
        let alpha_y = (0..12)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");
        let alpha_a = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y)).filter(|cell| cell.symbol() == "a"))
            .expect("alpha name should be rendered");
        assert_ne!(
            alpha_a.style().bg,
            Some(Color::DarkGray),
            "unselected row should not have selection bg"
        );
    }
}
