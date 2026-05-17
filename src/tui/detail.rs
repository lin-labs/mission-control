use crate::tui::app::WorkspaceState;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub fn render_detail(f: &mut Frame, area: Rect, ws: Option<&WorkspaceState>, scroll: u16, focused: bool) {
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let ws = match ws {
        Some(ws) => ws,
        None => {
            f.render_widget(
                Paragraph::new("No workspace selected").block(block),
                area,
            );
            return;
        }
    };

    let inner = block.inner(area);
    f.render_widget(block, area);

    let has_session = ws.session.is_some();
    let has_surfaces = !ws.surfaces.is_empty();
    let has_screen = ws.screen_preview.is_some();

    let mut constraints = vec![Constraint::Length(3)]; // header always

    if has_session {
        constraints.push(Constraint::Length(2)); // trajectory
    }

    if has_surfaces {
        let surface_lines = ws.surfaces.len() as u16 + 2; // title + items + blank
        constraints.push(Constraint::Length(surface_lines));
    }

    if has_session {
        constraints.push(Constraint::Min(4)); // progress/next steps
    }

    if has_screen {
        constraints.push(Constraint::Min(6)); // screen fills remaining space
    } else {
        constraints.push(Constraint::Min(1)); // filler
    }

    let chunks = Layout::vertical(constraints).split(inner);
    let mut chunk_idx = 0;

    render_header(f, chunks[chunk_idx], ws);
    chunk_idx += 1;

    if let Some(ref session) = ws.session {
        let traj_text = session
            .trajectory
            .as_deref()
            .unwrap_or("No trajectory yet");
        let traj = Paragraph::new(Line::from(vec![
            Span::styled("  ", Style::default().fg(Color::Cyan)),
            Span::raw(traj_text),
        ]));
        f.render_widget(traj, chunks[chunk_idx]);
        chunk_idx += 1;
    }

    if has_surfaces {
        render_surfaces(f, chunks[chunk_idx], &ws.surfaces);
        chunk_idx += 1;
    }

    if let Some(ref session) = ws.session {
        let mut lines: Vec<Line> = Vec::new();

        if !session.bullets.is_empty() {
            lines.push(Line::from(Span::styled(
                "Progress:",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            for bullet in &session.bullets {
                lines.push(Line::from(Span::styled(
                    format!("  - {}", bullet),
                    Style::default().fg(Color::Gray),
                )));
            }
        }

        if !session.next_steps.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Next Steps:",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            for step in &session.next_steps {
                let color = if step.contains("[x]") {
                    Color::DarkGray
                } else {
                    Color::Yellow
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}", step),
                    Style::default().fg(color),
                )));
            }
        }

        let body = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        f.render_widget(body, chunks[chunk_idx]);
        chunk_idx += 1;
    }

    if has_screen {
        if let Some(ref preview) = ws.screen_preview {
            let screen = Paragraph::new(preview.as_str())
                .block(
                    Block::default()
                        .title(" Screen ")
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .style(Style::default().fg(Color::DarkGray))
                .scroll((scroll, 0));
            f.render_widget(screen, chunks[chunk_idx]);
        }
    }
}

fn render_surfaces(f: &mut Frame, area: Rect, surfaces: &[crate::cmux::client::SurfaceInfo]) {
    let mut lines = vec![Line::from(Span::styled(
        "Surfaces:",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ))];
    for s in surfaces {
        lines.push(Line::from(vec![
            Span::styled("  ▸ ", Style::default().fg(Color::DarkGray)),
            Span::styled(&s.title, Style::default().fg(Color::Cyan)),
        ]));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_header(f: &mut Frame, area: Rect, ws: &WorkspaceState) {
    let status = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.status.as_deref())
        .unwrap_or_else(|| if ws.has_agent_surface() { "active" } else { "--" });

    let agent = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.agent.as_deref())
        .unwrap_or_else(|| {
            // Derive agent from surface titles
            ws.surfaces.iter().find_map(|s| {
                let t = s.title.to_lowercase();
                if t.contains("claude") { Some("claude") }
                else if t.contains("codex") { Some("codex") }
                else if t.contains("opencode") { Some("opencode") }
                else { None }
            }).unwrap_or("")
        });

    let host = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.host.as_deref())
        .unwrap_or("");

    let topic = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.topic.as_deref())
        .unwrap_or("");

    let header_line = Line::from(vec![
        Span::styled(
            format!(" {} ", ws.workspace.name),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {} ", status), status_color(status)),
        Span::styled(format!(" {} ", agent), Style::default().fg(Color::Cyan)),
        Span::styled(format!(" {} ", host), Style::default().fg(Color::Magenta)),
    ]);

    let topic_line = if !topic.is_empty() {
        Line::from(Span::styled(
            format!("  topic: {}", topic),
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::raw("")
    };

    let header = Paragraph::new(vec![header_line, topic_line]);
    f.render_widget(header, area);
}

fn status_color(status: &str) -> Style {
    match status {
        "active" => Style::default().fg(Color::Black).bg(Color::Green),
        "idle" => Style::default().fg(Color::Black).bg(Color::Yellow),
        "waiting" => Style::default().fg(Color::White).bg(Color::Red),
        "done" => Style::default().fg(Color::White).bg(Color::DarkGray),
        _ => Style::default().fg(Color::DarkGray),
    }
}
