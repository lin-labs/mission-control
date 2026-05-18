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
            // Spinner when an async refresh is in flight, otherwise status dot
            let (leader, leader_color) = if ws.loading {
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

            let line = Line::from(vec![
                Span::styled(format!("{} ", leader), Style::default().fg(leader_color)),
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
    use crate::tui::app::AgentState;
    match ws.agent_state() {
        AgentState::Working => ("\u{25cf}", Color::Green),     // ● baking
        AgentState::NeedsMe => ("\u{26a0}", Color::Yellow),    // ⚠ needs you
        AgentState::Idle    => ("\u{25cb}", Color::DarkGray),   // ○ nothing happening
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
