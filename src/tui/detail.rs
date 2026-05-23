use crate::tui::app::WorkspaceState;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub fn render_detail(
    f: &mut Frame,
    area: Rect,
    ws: Option<&WorkspaceState>,
    scroll: u16,
    focused: bool,
) {
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let ws = match ws {
        Some(ws) => ws,
        None => {
            f.render_widget(Paragraph::new("No workspace selected").block(block), area);
            return;
        }
    };

    // If a trajectory doc is available, delegate entirely to the trajectory view.
    // Fall through to the legacy rendering for workspaces without one.
    if let Some(doc) = ws.trajectory.as_ref() {
        crate::tui::trajectory_view::render(
            f,
            area,
            Some(doc),
            scroll,
            focused,
            ws.edit_state.as_ref(),
        );
        return;
    }

    let inner = block.inner(area);
    f.render_widget(block, area);

    // The detail panel answers three questions:
    //   1. What did I ask?  (user prompt)
    //   2. What's happening? (activity, tasks, trajectory)
    //   3. What should I focus on next? (notes, next steps)
    let mut lines: Vec<Line> = Vec::new();

    // ── Header ──────────────────────────────────────────
    render_header_lines(&mut lines, ws);
    lines.push(Line::raw(""));

    // ── 1. My Ask ───────────────────────────────────────
    if let Some(ref prompt) = ws.screen_insights.user_prompt {
        lines.push(Line::from(vec![
            Span::styled("  › ", Style::default().fg(Color::Yellow)),
            Span::styled(
                prompt.as_str(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::raw(""));
    }

    // ── 2. What's Happening ─────────────────────────────
    let mut has_status = false;
    if let Some(ref activity) = ws.screen_insights.activity {
        lines.push(Line::from(Span::styled(
            format!("  {}", activity),
            Style::default().fg(Color::Green),
        )));
        has_status = true;
    } else if let Some(ref dur) = ws.screen_insights.duration {
        lines.push(Line::from(Span::styled(
            format!("  Worked for {}", dur),
            Style::default().fg(Color::DarkGray),
        )));
        has_status = true;
    }

    if ws.screen_insights.tasks_total > 0 {
        let mut task_spans = vec![Span::styled(
            format!(
                "  Tasks: {}/{} ✔",
                ws.screen_insights.tasks_done, ws.screen_insights.tasks_total
            ),
            Style::default().fg(Color::Cyan),
        )];
        if let Some(ref pending) = ws.screen_insights.pending_task {
            task_spans.push(Span::styled(
                format!("  — {}", pending),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::from(task_spans));
        has_status = true;
    }

    // Trajectory (LLM summary — prefer in-memory summary, fall back to session file)
    let trajectory: Option<&str> = ws
        .summary
        .as_ref()
        .map(|s| s.trajectory.as_str())
        .or_else(|| {
            ws.session
                .as_ref()
                .and_then(|s| s.trajectory.as_deref())
        });
    if let Some(traj) = trajectory {
        lines.push(Line::from(Span::styled(
            format!("  {}", traj),
            Style::default().fg(Color::Cyan),
        )));
        has_status = true;
    } else if ws.summarizing {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                format!("{} ", crate::tui::sidebar::spinner_frame()),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                "summarizing via codex…",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        has_status = true;
    }

    if has_status {
        lines.push(Line::raw(""));
    }

    // ── 3. Notes (persistent, user-written) ─────────────
    lines.push(Line::from(Span::styled(
        "─── Notes (n to edit) ─────────────────────────",
        Style::default().fg(Color::DarkGray),
    )));
    if let Some(ref notes) = ws.notes {
        for note_line in notes.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", note_line),
                Style::default().fg(Color::White),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "  (no notes yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::raw(""));

    // ── Progress / Next Steps (from session file) ───────
    if let Some(ref session) = ws.session {
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
            lines.push(Line::raw(""));
        }

    }

    // Next Steps — prefer in-memory summary, fall back to session file
    let next_steps: &[String] = ws
        .summary
        .as_ref()
        .map(|s| s.next_steps.as_slice())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ws.session
                .as_ref()
                .map(|s| s.next_steps.as_slice())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or(&[]);
    if !next_steps.is_empty() {
        lines.push(Line::from(Span::styled(
            "Next Steps:",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        for step in next_steps {
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
        lines.push(Line::raw(""));
    }

    // ── Screen preview (raw, least important) ───────────
    if let Some(ref preview) = ws.screen_preview {
        let non_blank = preview
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        if non_blank > 2 {
            lines.push(Line::from(Span::styled(
                "─── Screen ────────────────────────────────────",
                Style::default().fg(Color::DarkGray),
            )));
            for screen_line in preview.lines() {
                lines.push(Line::from(Span::styled(
                    screen_line,
                    Style::default().fg(Color::Gray),
                )));
            }
        }
    }

    let content = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(content, inner);
}

fn render_header_lines<'a>(lines: &mut Vec<Line<'a>>, ws: &'a WorkspaceState) {
    let state = ws.agent_state();
    let status = state.label();
    let spinner = if ws.loading {
        Some(crate::tui::sidebar::spinner_frame())
    } else {
        None
    };

    let agent = ws.agent_name();
    let host = ws.host_name();
    let dir = ws.working_dir();

    // Line 1: name + status badge + agent + host
    let mut header_spans = vec![
        Span::styled(
            format!(" {} ", ws.workspace.name),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {} ", status), status_color(status)),
    ];
    if let Some(s) = spinner {
        header_spans.push(Span::styled(
            format!(" {} ", s),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !agent.is_empty() {
        header_spans.push(Span::styled(
            format!(" {} ", agent),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !host.is_empty() {
        header_spans.push(Span::styled(
            format!(" {} ", host),
            Style::default().fg(Color::Magenta),
        ));
    }
    if let Some(ref dur) = ws.screen_insights.duration {
        header_spans.push(Span::styled(
            format!(" {}  ", dur),
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::from(header_spans));

    // Line 2: working directory and/or topic
    let topic = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.topic.as_deref())
        .unwrap_or("");

    if !dir.is_empty() || !topic.is_empty() {
        let mut sub_spans = Vec::new();
        if !dir.is_empty() {
            sub_spans.push(Span::styled(
                format!("  {}", dir),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if !topic.is_empty() {
            if !dir.is_empty() {
                sub_spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            } else {
                sub_spans.push(Span::styled("  ", Style::default()));
            }
            sub_spans.push(Span::styled(
                topic,
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(sub_spans));
    }
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
