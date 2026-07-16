//! Pure state machine and renderer for verified cross-device handoff.

use crate::mc_data::arcmux_handoff::{
    HandoffFailure, HandoffPlan, HandoffSourceContext, HandoffStage, HandoffSuccess, HandoffTarget,
    HandoffUpdateKind,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffView {
    PickTarget,
    Confirm,
    Running(HandoffStage),
    Success(HandoffSuccess),
    Failure(HandoffFailure),
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffOutcome {
    Handled,
    Cancel,
    Start(Box<HandoffPlan>),
}

#[derive(Debug, Clone)]
pub struct HandoffModal {
    pub workspace_uuid: String,
    pub surface_label: String,
    pub generation: u64,
    pub view: HandoffView,
    source: Option<HandoffSourceContext>,
    peers: Vec<String>,
    profiles: Vec<String>,
    peer_index: usize,
    profile_index: usize,
    last_plan: Option<HandoffPlan>,
}

impl HandoffModal {
    pub fn new(
        source: HandoffSourceContext,
        surface_label: String,
        mut peers: Vec<String>,
    ) -> Self {
        peers.sort();
        peers.dedup();
        let mut profiles = vec![source.agent.clone()];
        for profile in ["codex", "claude", "grok"] {
            if !profiles.iter().any(|candidate| candidate == profile) {
                profiles.push(profile.to_string());
            }
        }
        let view = if peers.is_empty() {
            HandoffView::Unavailable("no connected handoff-capable peers".to_string())
        } else {
            HandoffView::PickTarget
        };
        Self {
            workspace_uuid: source.workspace_uuid.clone(),
            surface_label,
            generation: 0,
            view,
            source: Some(source),
            peers,
            profiles,
            peer_index: 0,
            profile_index: 0,
            last_plan: None,
        }
    }

    pub fn unavailable(
        workspace_uuid: String,
        surface_label: String,
        message: impl Into<String>,
    ) -> Self {
        Self {
            workspace_uuid,
            surface_label,
            generation: 0,
            view: HandoffView::Unavailable(message.into()),
            source: None,
            peers: Vec::new(),
            profiles: Vec::new(),
            peer_index: 0,
            profile_index: 0,
            last_plan: None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> HandoffOutcome {
        match &self.view {
            HandoffView::PickTarget => self.handle_pick_target(key),
            HandoffView::Confirm => match key.code {
                KeyCode::Esc => {
                    self.view = HandoffView::PickTarget;
                    HandoffOutcome::Handled
                }
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => self
                    .selected_plan()
                    .map(Box::new)
                    .map(HandoffOutcome::Start)
                    .unwrap_or(HandoffOutcome::Handled),
                _ => HandoffOutcome::Handled,
            },
            HandoffView::Running(_) => HandoffOutcome::Handled,
            HandoffView::Success(_) | HandoffView::Unavailable(_) => match key.code {
                KeyCode::Esc | KeyCode::Enter => HandoffOutcome::Cancel,
                _ => HandoffOutcome::Handled,
            },
            HandoffView::Failure(failure) => match key.code {
                KeyCode::Esc | KeyCode::Enter => HandoffOutcome::Cancel,
                KeyCode::Char('r') | KeyCode::Char('R') if failure.retryable => self
                    .last_plan
                    .clone()
                    .map(Box::new)
                    .map(HandoffOutcome::Start)
                    .unwrap_or(HandoffOutcome::Handled),
                _ => HandoffOutcome::Handled,
            },
        }
    }

    fn handle_pick_target(&mut self, key: KeyEvent) -> HandoffOutcome {
        match key.code {
            KeyCode::Esc => HandoffOutcome::Cancel,
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.peers.is_empty() {
                    self.peer_index = (self.peer_index + 1) % self.peers.len();
                }
                HandoffOutcome::Handled
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.peers.is_empty() {
                    self.peer_index = self
                        .peer_index
                        .checked_sub(1)
                        .unwrap_or(self.peers.len() - 1);
                }
                HandoffOutcome::Handled
            }
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('p') => {
                if !self.profiles.is_empty() {
                    self.profile_index = (self.profile_index + 1) % self.profiles.len();
                }
                HandoffOutcome::Handled
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if !self.profiles.is_empty() {
                    self.profile_index = self
                        .profile_index
                        .checked_sub(1)
                        .unwrap_or(self.profiles.len() - 1);
                }
                HandoffOutcome::Handled
            }
            KeyCode::Char(value) if value.is_ascii_digit() && value != '0' => {
                let index = (value as u8 - b'1') as usize;
                if index < self.peers.len() {
                    self.peer_index = index;
                    self.view = HandoffView::Confirm;
                }
                HandoffOutcome::Handled
            }
            KeyCode::Enter => {
                if self.selected_plan().is_some() {
                    self.view = HandoffView::Confirm;
                }
                HandoffOutcome::Handled
            }
            _ => HandoffOutcome::Handled,
        }
    }

    fn selected_plan(&self) -> Option<HandoffPlan> {
        Some(HandoffPlan {
            source: self.source.clone()?,
            target: HandoffTarget {
                peer_id: self.peers.get(self.peer_index)?.clone(),
                profile: self.profiles.get(self.profile_index)?.clone(),
            },
        })
    }

    pub fn mark_running(&mut self, generation: u64, plan: HandoffPlan) {
        self.generation = generation;
        self.last_plan = Some(plan);
        self.view = HandoffView::Running(HandoffStage::Preparing);
    }

    pub fn invalidate_confirmation(&mut self, message: impl Into<String>) {
        self.source = None;
        self.last_plan = None;
        self.view = HandoffView::Unavailable(message.into());
    }

    pub fn apply_update(&mut self, generation: u64, kind: HandoffUpdateKind) {
        if generation != self.generation {
            return;
        }
        self.view = match kind {
            HandoffUpdateKind::Progress(stage) => HandoffView::Running(stage),
            HandoffUpdateKind::Finished(Ok(success)) => HandoffView::Success(success),
            HandoffUpdateKind::Finished(Err(failure)) => HandoffView::Failure(failure),
        };
    }

    fn selected_peer(&self) -> Option<&str> {
        self.peers.get(self.peer_index).map(String::as_str)
    }

    fn selected_profile(&self) -> Option<&str> {
        self.profiles.get(self.profile_index).map(String::as_str)
    }
}

pub fn render(frame: &mut Frame, area: Rect, modal: &HandoffModal) {
    let height = match modal.view {
        HandoffView::Failure(_) => 11,
        HandoffView::Success(_) => 9,
        _ => (modal.peers.len().min(9) as u16 + 7).clamp(8, 14),
    };
    if area.height <= height || area.width < 24 {
        return;
    }
    let rect = Rect {
        x: area.x,
        y: area.y + area.height - height,
        width: area.width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(" Verified handoff ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(lines(modal)).wrap(Wrap { trim: true }),
        inner,
    );
}

fn lines(modal: &HandoffModal) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("  source: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            truncate(&modal.surface_label, 70),
            Style::default().fg(Color::White),
        ),
    ])];
    match &modal.view {
        HandoffView::PickTarget => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  target profile: {}  [h/l or p to change]",
                    modal.selected_profile().unwrap_or("unavailable")
                ),
                Style::default().fg(Color::Cyan),
            )));
            for (index, peer) in modal.peers.iter().enumerate().take(9) {
                let selected = index == modal.peer_index;
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  [{}] ", index + 1),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(
                        format!("{} · {}", peer, modal.selected_profile().unwrap_or("?")),
                        Style::default()
                            .fg(if selected { Color::White } else { Color::Gray })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                ]));
            }
            lines.push(hint("Enter confirm · Esc cancel"));
        }
        HandoffView::Confirm => {
            lines.push(Line::from(Span::styled(
                format!(
                    "  move to {} · {}?",
                    modal.selected_peer().unwrap_or("?"),
                    modal.selected_profile().unwrap_or("?")
                ),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                "  Source remains alive until target context is verified.",
                Style::default().fg(Color::Gray),
            )));
            lines.push(hint("Enter/y start · Esc back"));
        }
        HandoffView::Running(stage) => {
            lines.push(Line::from(Span::styled(
                format!("  … {}", stage.label()),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(Span::styled(
                "  Source is still running.",
                Style::default().fg(Color::Gray),
            )));
        }
        HandoffView::Success(success) => {
            lines.push(Line::from(Span::styled(
                "  ✓ target context loaded; exact source retired",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(format!(
                "  target: {}",
                success.target_locator.display()
            )));
            lines.push(Line::from(format!(
                "  source: {} (retired)",
                success.source_locator.display()
            )));
            lines.push(hint("Enter/Esc close"));
        }
        HandoffView::Failure(failure) => {
            lines.push(Line::from(Span::styled(
                format!("  ✗ {}", failure.message),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(format!(
                "  source: {} (still live)",
                failure.source_locator.display()
            )));
            if let Some(target) = failure.target_locator.as_ref() {
                lines.push(Line::from(format!("  target: {}", target.display())));
            }
            if let Some(handoff_id) = failure.handoff_id.as_deref() {
                lines.push(Line::from(format!(
                    "  reconcile: arcmux handoff show {handoff_id}"
                )));
            }
            if failure.target_uncertain {
                lines.push(Line::from(Span::styled(
                    "  ⚠ duplicate-live possible: target existence is uncertain",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
            } else if failure.duplicate_live {
                lines.push(Line::from(Span::styled(
                    "  ⚠ duplicate-live: target exists and source was preserved",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            lines.push(hint(if failure.retryable {
                "r retry · Enter/Esc close"
            } else {
                "Enter/Esc close"
            }));
        }
        HandoffView::Unavailable(message) => {
            lines.push(Line::from(Span::styled(
                format!("  unavailable: {message}"),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(Span::styled(
                "  Source is unchanged.",
                Style::default().fg(Color::Gray),
            )));
            lines.push(hint("Enter/Esc close"));
        }
    }
    lines
}

fn hint(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::default().fg(Color::DarkGray),
    ))
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    value
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mc_data::arcmux_handoff::{HandoffLocator, HandoffUpdateKind};
    use crate::mc_data::arcmux_mesh::RemoteSessionLocator;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn source() -> HandoffSourceContext {
        HandoffSourceContext {
            workspace_uuid: "workspace-1".into(),
            surface_uuid: "11111111-1111-4111-8111-111111111111".into(),
            locator: RemoteSessionLocator {
                schema_version: 1,
                device_id: "ref".into(),
                profile_scope: "root".into(),
                session_id: "s-source".into(),
                transport_binding_id: None,
            },
            agent: "claude".into(),
            project: "mission-control".into(),
            goal: "Continue safely".into(),
            history: "2026-07-15-20-handoff.md".into(),
            conversation_id: None,
            parent_handoff_id: None,
            validation: "not_run".into(),
            observation: crate::mc_data::arcmux_handoff::HandoffSourceObservation {
                turn_count: 1,
                updated_at: "2026-07-15T20:01:00-07:00".into(),
                last_turn_end_at: Some("2026-07-15T20:01:00-07:00".into()),
            },
        }
    }

    #[test]
    fn peers_are_sorted_and_source_agent_is_default_profile() {
        let modal = HandoffModal::new(
            source(),
            "claude source".into(),
            vec!["zeta".into(), "alpha".into(), "alpha".into()],
        );
        assert_eq!(modal.peers, vec!["alpha", "zeta"]);
        assert_eq!(modal.selected_profile(), Some("claude"));
    }

    #[test]
    fn pick_then_confirm_produces_exact_plan() {
        let mut modal = HandoffModal::new(source(), "claude source".into(), vec!["devbox".into()]);
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            HandoffOutcome::Handled
        );
        assert!(matches!(modal.view, HandoffView::Confirm));
        let HandoffOutcome::Start(plan) = modal.handle_key(key(KeyCode::Enter)) else {
            panic!("confirmation did not start")
        };
        assert_eq!(plan.source.locator.session_id, "s-source");
        assert_eq!(plan.target.peer_id, "devbox");
        assert_eq!(plan.target.profile, "claude");
    }

    #[test]
    fn running_absorbs_escape_and_stale_generation_is_ignored() {
        let mut modal = HandoffModal::new(source(), "source".into(), vec!["devbox".into()]);
        let plan = modal.selected_plan().unwrap();
        modal.mark_running(9, plan);
        assert_eq!(modal.handle_key(key(KeyCode::Esc)), HandoffOutcome::Handled);
        modal.apply_update(8, HandoffUpdateKind::Progress(HandoffStage::Launching));
        assert!(matches!(
            modal.view,
            HandoffView::Running(HandoffStage::Preparing)
        ));
    }

    #[test]
    fn duplicate_live_failure_cannot_blindly_retry() {
        let mut modal = HandoffModal::new(source(), "source".into(), vec!["devbox".into()]);
        let plan = modal.selected_plan().unwrap();
        modal.mark_running(3, plan);
        modal.apply_update(
            3,
            HandoffUpdateKind::Finished(Err(HandoffFailure {
                message: "retire failed".into(),
                handoff_id: Some("handoff-1".into()),
                source_locator: HandoffLocator {
                    device_id: "ref".into(),
                    profile_scope: "root".into(),
                    session_id: "s-source".into(),
                },
                target_locator: Some(HandoffLocator {
                    device_id: "devbox".into(),
                    profile_scope: "root".into(),
                    session_id: "s-target".into(),
                }),
                target_uncertain: false,
                duplicate_live: true,
                retryable: false,
            })),
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Char('r'))),
            HandoffOutcome::Handled
        );
    }

    #[test]
    fn uncertain_target_shows_reconciliation_id_and_cannot_retry() {
        let mut modal = HandoffModal::new(source(), "source".into(), vec!["devbox".into()]);
        let plan = modal.selected_plan().unwrap();
        modal.mark_running(4, plan);
        modal.apply_update(
            4,
            HandoffUpdateKind::Finished(Err(HandoffFailure {
                message: "arcmux handoff command timed out".into(),
                handoff_id: Some("handoff-uncertain".into()),
                source_locator: HandoffLocator {
                    device_id: "ref".into(),
                    profile_scope: "root".into(),
                    session_id: "s-source".into(),
                },
                target_locator: None,
                target_uncertain: true,
                duplicate_live: true,
                retryable: false,
            })),
        );

        let rendered = lines(&modal)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("arcmux handoff show handoff-uncertain"));
        assert!(rendered.contains("target existence is uncertain"));
        assert_eq!(
            modal.handle_key(key(KeyCode::Char('r'))),
            HandoffOutcome::Handled
        );
    }

    #[test]
    fn raw_surface_unavailable_message_is_terminal() {
        let mut modal = HandoffModal::unavailable(
            "workspace-1".into(),
            "raw codex".into(),
            "not arcmux-supervised",
        );
        assert!(matches!(modal.view, HandoffView::Unavailable(_)));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            HandoffOutcome::Cancel
        );
    }
}
