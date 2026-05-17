use crate::tui::app::WorkspaceState;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

pub fn render_sidebar(
    f: &mut Frame,
    area: Rect,
    workspaces: &[WorkspaceState],
    selected: usize,
    focused: bool,
) {
    let items: Vec<ListItem> = workspaces
        .iter()
        .map(|ws| {
            let (dot, dot_color) = status_indicator(ws);
            let host_badge = ws
                .session
                .as_ref()
                .and_then(|s| s.frontmatter.host.as_deref())
                .filter(|h| *h != "mbp")
                .map(|h| format!(" [{}]", h))
                .unwrap_or_default();

            let line = Line::from(vec![
                Span::styled(format!("{} ", dot), Style::default().fg(dot_color)),
                Span::styled(
                    ws.workspace.name.clone(),
                    Style::default().fg(Color::White),
                ),
                Span::styled(host_badge, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Workspaces ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .bg(Color::DarkGray),
        );

    let mut state = ListState::default();
    state.select(Some(selected));

    f.render_stateful_widget(list, area, &mut state);
}

fn status_indicator(ws: &WorkspaceState) -> (&str, Color) {
    // Check session status first
    if let Some(status) = ws.session.as_ref().and_then(|s| s.frontmatter.status.as_deref()) {
        return match status {
            "active" => ("\u{25cf}", Color::Green),    // ● filled circle
            "idle" => ("\u{25d0}", Color::Yellow),     // ◐ half circle
            "waiting" => ("\u{26a0}", Color::Red),     // ⚠ warning
            "done" => ("\u{25cb}", Color::DarkGray),   // ○ empty circle
            _ => ("\u{25cb}", Color::DarkGray),
        };
    }
    // Fallback: check if surfaces indicate an agent is running
    if ws.has_agent_surface() {
        ("\u{25cf}", Color::Green) // ● agent surface present
    } else {
        ("\u{25cb}", Color::DarkGray) // ○ no agent
    }
}
