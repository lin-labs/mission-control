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

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Workspaces ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
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
    match ws.session.as_ref().and_then(|s| s.frontmatter.status.as_deref()) {
        Some("active") => ("\u{25cf}", Color::Green),    // filled circle
        Some("idle") => ("\u{25d0}", Color::Yellow),     // half circle
        Some("waiting") => ("\u{26a0}", Color::Red),     // warning
        Some("done") => ("\u{25cb}", Color::DarkGray),   // empty circle
        _ => ("\u{25cb}", Color::DarkGray),              // empty circle, no agent
    }
}
