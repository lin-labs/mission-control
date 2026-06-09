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
    // Inner width = area width minus 2 outer-border columns.
    let inner_width = area.width.saturating_sub(2);
    // Column 0 is reserved for the selection marker (▶ on the selected row,
    // space otherwise). The rest of the row is the workspace's colored body.
    let body_width = inner_width.saturating_sub(1);

    let items: Vec<ListItem> = workspaces
        .iter()
        .enumerate()
        .map(|(idx, ws)| {
            let is_selected = idx == selected;

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

            let accent_color =
                crate::sidebar_pure::workspace_accent_color(ws.workspace.custom_color.as_deref())
                    .unwrap_or(Color::DarkGray);

            // Truncate name to fit between a leading 1-space pad, leader, and
            // host_badge inside the body.
            let name_max = (body_width as usize)
                .saturating_sub(1) // leading pad inside the body
                .saturating_sub(leader_str.chars().count())
                .saturating_sub(host_badge.chars().count());
            let display_name = truncate_for_width(&ws.workspace.name, name_max as u16);

            let used = 1
                + leader_str.chars().count()
                + display_name.chars().count()
                + host_badge.chars().count();
            let pad = " ".repeat((body_width as usize).saturating_sub(used));

            // Selection signal: a ▶ marker in column 0 (outside the colored
            // body) on the selected row, plus Modifier::BOLD on the name so
            // it still reads as selected even on monochrome terminals.
            let marker = if is_selected { "▶" } else { " " };
            let marker_style = Style::default().fg(if is_selected {
                Color::White
            } else {
                Color::Reset
            });

            let name_modifiers = if is_selected {
                Modifier::BOLD | Modifier::UNDERLINED
            } else {
                Modifier::BOLD
            };

            let line = Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(" ", Style::default().bg(accent_color)),
                Span::styled(
                    leader_str,
                    Style::default().fg(leader_color).bg(accent_color),
                ),
                Span::styled(
                    display_name,
                    Style::default()
                        .fg(Color::White)
                        .bg(accent_color)
                        .add_modifier(name_modifiers),
                ),
                Span::styled(
                    host_badge,
                    Style::default().fg(Color::Gray).bg(accent_color),
                ),
                Span::styled(pad, Style::default().bg(accent_color)),
            ]);

            ListItem::new(line)
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
        // Selection styling is in the per-row spans; suppress the List's
        // default highlight so it doesn't paint over them.
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
                window_id: Some("window-test".to_string()),
                window_ref: Some("window:1".to_string()),
                ref_id: "workspace:1".to_string(),
                uuid: "workspace-1".to_string(),
                name: name.to_string(),
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
            mux_status: None,
            classification: None,
            loading: false,
            summary: None,
            beads: None,
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
    fn each_workspace_is_exactly_one_row() {
        let workspaces = vec![
            test_workspace_state("alpha", Some("#C0392B")),
            test_workspace_state("beta", Some("#006B6B")),
            test_workspace_state("gamma", Some("#4A5C18")),
        ];
        let backend = TestBackend::new(30, 8);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 8), &workspaces, 0, true))
            .unwrap();

        let alpha_y = (0..8)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");
        let beta_y = (0..8)
            .find(|y| buf_row(&terminal, *y).contains("beta"))
            .expect("beta should render");
        let gamma_y = (0..8)
            .find(|y| buf_row(&terminal, *y).contains("gamma"))
            .expect("gamma should render");

        // Adjacent workspaces are exactly 1 row apart (no gap, no borders).
        assert_eq!(beta_y - alpha_y, 1, "beta should be 1 row below alpha");
        assert_eq!(gamma_y - beta_y, 1, "gamma should be 1 row below beta");
    }

    #[test]
    fn workspace_row_fills_with_accent_bg() {
        let workspaces = vec![test_workspace_state("alpha", Some("#C0392B"))];
        let backend = TestBackend::new(30, 5);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 5), &workspaces, 0, true))
            .unwrap();

        let alpha_y = (0..5)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");

        let buf = terminal.backend().buffer();
        let name_a = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y)).filter(|cell| cell.symbol() == "a"))
            .expect("alpha name should be rendered");
        assert_eq!(
            name_a.style().bg,
            Some(Color::Rgb(0xC0, 0x39, 0x2B)),
            "content bg should be the workspace's accent color"
        );
    }

    #[test]
    fn selected_workspace_shows_marker_and_underline() {
        let workspaces = vec![
            test_workspace_state("alpha", Some("#C0392B")),
            test_workspace_state("beta", Some("#006B6B")),
        ];
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 6), &workspaces, 1, true))
            .unwrap();

        let beta_y = (0..6)
            .find(|y| buf_row(&terminal, *y).contains("beta"))
            .expect("beta should render");

        // Selected row starts with ▶ in col 0 of the inner area (col 1 of screen).
        let row = buf_row(&terminal, beta_y);
        assert!(
            row.contains('▶'),
            "selected row should contain ▶ marker, got: {:?}",
            row
        );

        let buf = terminal.backend().buffer();
        let beta_b = (0..buf.area.width)
            .find_map(|x| buf.cell((x, beta_y)).filter(|cell| cell.symbol() == "b"))
            .expect("beta name should be rendered");
        assert!(
            beta_b.style().add_modifier.contains(Modifier::UNDERLINED),
            "selected workspace name should be underlined"
        );

        // Unselected (alpha) has no ▶ and no underline on the name.
        let alpha_y = (0..6)
            .find(|y| buf_row(&terminal, *y).contains("alpha"))
            .expect("alpha should render");
        let alpha_row = buf_row(&terminal, alpha_y);
        assert!(
            !alpha_row.contains('▶'),
            "unselected row should not contain ▶, got: {:?}",
            alpha_row
        );
        let alpha_a = (0..buf.area.width)
            .find_map(|x| buf.cell((x, alpha_y)).filter(|cell| cell.symbol() == "a"))
            .expect("alpha name should be rendered");
        assert!(
            !alpha_a.style().add_modifier.contains(Modifier::UNDERLINED),
            "unselected workspace name should not be underlined"
        );
    }

    #[test]
    fn workspace_without_custom_color_uses_dark_gray_bg() {
        let workspaces = vec![test_workspace_state("plain", None)];
        let backend = TestBackend::new(30, 5);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| render_sidebar(f, Rect::new(0, 0, 30, 5), &workspaces, 0, true))
            .unwrap();

        let y = (0..5)
            .find(|y| buf_row(&terminal, *y).contains("plain"))
            .expect("plain should render");
        let buf = terminal.backend().buffer();

        let name_p = (0..buf.area.width)
            .find_map(|x| buf.cell((x, y)).filter(|cell| cell.symbol() == "p"))
            .expect("plain name should be rendered");
        assert_eq!(name_p.style().bg, Some(Color::DarkGray));
    }
}
