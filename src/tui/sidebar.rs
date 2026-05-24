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
    // Inner width = area width minus 2 border columns.
    let inner_width = area.width.saturating_sub(2);

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

            // Tint the workspace name with cmux's user-set color when present.
            // Falls back to white so workspaces without a color render unchanged.
            let name_color = ws
                .workspace
                .custom_color
                .as_deref()
                .and_then(crate::sidebar_pure::parse_hex_color)
                .unwrap_or(Color::White);

            let name_line = Line::from(vec![
                Span::styled(format!("{} ", leader), Style::default().fg(leader_color)),
                Span::styled(
                    ws.workspace.name.clone(),
                    Style::default()
                        .fg(name_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(host_badge, Style::default().fg(Color::DarkGray)),
            ]);

            // Optionally render a dim description subtitle line.
            if let Some(sub) =
                description_subtitle_line(ws.workspace.description.as_deref(), inner_width)
            {
                ListItem::new(vec![name_line, sub])
            } else {
                ListItem::new(name_line)
            }
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
    let max_text_cols = sidebar_inner_width.saturating_sub(indent_len).saturating_sub(1);
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
