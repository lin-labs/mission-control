use crate::tui::app::WorkspaceState;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub fn render_detail(f: &mut Frame, area: Rect, ws: Option<&WorkspaceState>) {
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

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
    let has_screen = ws.show_screen && ws.screen_preview.is_some();

    let constraints = if has_session && has_screen {
        vec![
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(12),
        ]
    } else if has_session {
        vec![
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(4),
        ]
    } else if has_screen {
        vec![Constraint::Length(3), Constraint::Min(4)]
    } else {
        vec![Constraint::Length(3), Constraint::Min(1)]
    };

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

        let mut lines: Vec<Line> = Vec::new();

        if !session.bullets.is_empty() {
            lines.push(Line::from(Span::styled(
                "Progress:",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            for bullet in &session.bullets {
                lines.push(Line::from(format!("  - {}", bullet)));
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
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(screen, chunks[chunk_idx]);
        }
    } else if !has_session {
        let hint = Paragraph::new("No agent session. Press 's' for screen preview.");
        f.render_widget(hint, chunks[chunk_idx]);
    }
}

fn render_header(f: &mut Frame, area: Rect, ws: &WorkspaceState) {
    let status = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.status.as_deref())
        .unwrap_or("--");

    let agent = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.agent.as_deref())
        .unwrap_or("");

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
