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

    let items: Vec<ListItem> = workspaces
        .iter()
        .enumerate()
        .map(|(idx, ws)| {
            let is_selected = idx == selected;

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

            let border_color = crate::sidebar_pure::workspace_accent_color(
                ws.workspace.custom_color.as_deref(),
            )
            .unwrap_or(Color::DarkGray);

            // Truncate name to fit between leader and host_badge inside the box.
            let name_max = (text_width as usize)
                .saturating_sub(leader_str.chars().count())
                .saturating_sub(host_badge.chars().count());
            let display_name = truncate_for_width(&ws.workspace.name, name_max as u16);

            // Compute the right-pad needed to fill the box width.
            let used =
                1 + leader_str.chars().count() + display_name.chars().count() + host_badge.chars().count();
            let pad = " ".repeat((bar_width as usize).saturating_sub(used));

            let bar = "━".repeat(bar_width as usize);
            let border_style = Style::default().fg(border_color);
            let content_bg = if is_selected {
                Color::DarkGray
            } else {
                Color::Reset
            };

            let top_line = Line::from(Span::styled(format!("┏{}┓", bar), border_style));
            let bot_line = Line::from(Span::styled(format!("┗{}┛", bar), border_style));

            let content_line = Line::from(vec![
                Span::styled("┃", border_style),
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
                Span::styled("┃", border_style),
            ]);

            ListItem::new(vec![top_line, content_line, bot_line])
        })
        .collect();

    let outer_border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let list = List::new(items).block(
        Block::default()
            .title(" Workspaces ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(outer_border_color)),
    );

    let mut state = ListState::default();
    state.select(Some(selected));

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
    fn workspace_renders_inside_heavy_box_with_accent_color() {
        let workspaces = vec![test_workspace_state("alpha", Some("#C0392B"))];
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 8), &workspaces, 0, true))
            .unwrap();

        // Find the row containing "alpha".
        let mut alpha_row: Option<u16> = None;
        for y in 0..8 {
            if buf_row(&terminal, y).contains("alpha") {
                alpha_row = Some(y);
                break;
            }
        }
        let y = alpha_row.expect("workspace name was not rendered");

        // Row above must be a heavy top border ┏━━…━━┓ in the accent color.
        let above = buf_row(&terminal, y - 1);
        assert!(
            above.contains('┏') && above.contains('┓'),
            "expected ┏ … ┓ top border above the name row, got: {:?}",
            above
        );

        // Row below must be a heavy bottom border ┗━━…━━┛.
        let below = buf_row(&terminal, y + 1);
        assert!(
            below.contains('┗') && below.contains('┛'),
            "expected ┗ … ┛ bottom border below the name row, got: {:?}",
            below
        );

        // The border ┏ on the top row should be tinted with the workspace's
        // custom color (#C0392B → rgb(192, 57, 43)).
        let buf = terminal.backend().buffer();
        let top_corner = (0..buf.area.width)
            .find_map(|x| buf.cell((x, y - 1)).filter(|cell| cell.symbol() == "┏"))
            .expect("top-left corner should be present");
        assert_eq!(top_corner.style().fg, Some(Color::Rgb(0xC0, 0x39, 0x2B)));

        // The name itself should be bold White.
        let name_cell = (0..buf.area.width)
            .find_map(|x| buf.cell((x, y)).filter(|cell| cell.symbol() == "a"))
            .expect("workspace name should be rendered");
        assert_eq!(name_cell.style().fg, Some(Color::White));
    }

    #[test]
    fn selected_workspace_fills_content_with_dark_gray_bg() {
        let workspaces = vec![
            test_workspace_state("alpha", Some("#C0392B")),
            test_workspace_state("beta", Some("#006B6B")),
        ];
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        // Select beta (idx 1).
        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 10), &workspaces, 1, true))
            .unwrap();

        // Find the content row of beta.
        let mut beta_row: Option<u16> = None;
        for y in 0..10 {
            if buf_row(&terminal, y).contains("beta") {
                beta_row = Some(y);
                break;
            }
        }
        let y = beta_row.expect("beta should render");

        // The name cell ("b") should have DarkGray bg (selection fill).
        let buf = terminal.backend().buffer();
        let beta_b = (0..buf.area.width)
            .find_map(|x| buf.cell((x, y)).filter(|cell| cell.symbol() == "b"))
            .expect("beta should be in buffer");
        assert_eq!(
            beta_b.style().bg,
            Some(Color::DarkGray),
            "selected row content should have DarkGray background"
        );

        // The border characters on beta's box should NOT have bg fill.
        let beta_left_border = (0..buf.area.width)
            .find_map(|x| buf.cell((x, y)).filter(|cell| cell.symbol() == "┃"))
            .expect("beta box left ┃ should be present");
        assert_ne!(
            beta_left_border.style().bg,
            Some(Color::DarkGray),
            "border characters should not get the selection bg fill"
        );

        // The non-selected workspace (alpha) should NOT have bg fill on its name.
        let mut alpha_row: Option<u16> = None;
        for yy in 0..10 {
            if buf_row(&terminal, yy).contains("alpha") {
                alpha_row = Some(yy);
                break;
            }
        }
        let ay = alpha_row.expect("alpha should render");
        let alpha_a = (0..buf.area.width)
            .find_map(|x| buf.cell((x, ay)).filter(|cell| cell.symbol() == "a"))
            .expect("alpha should be in buffer");
        assert_ne!(
            alpha_a.style().bg,
            Some(Color::DarkGray),
            "unselected row should not have selection bg"
        );
    }

    #[test]
    fn workspace_without_custom_color_uses_dark_gray_border() {
        let workspaces = vec![test_workspace_state("plain", None)];
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 6), &workspaces, 0, true))
            .unwrap();

        // The border characters should be DarkGray when no custom_color is set.
        let buf = terminal.backend().buffer();
        let any_border = (0..buf.area.width)
            .flat_map(|x| (0..buf.area.height).map(move |y| (x, y)))
            .find_map(|(x, y)| buf.cell((x, y)).filter(|cell| cell.symbol() == "┃"))
            .expect("at least one ┃ border should be rendered");
        assert_eq!(any_border.style().fg, Some(Color::DarkGray));
    }
}
