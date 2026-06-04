//! Goal dispatch modal (T4).
//!
//! When the user presses Enter on a populated goal row in `## Beads`
//! the modal opens at the bottom of the detail pane and offers:
//!
//!   - numeric shortcuts (1..=9) for the existing terminal surfaces in the
//!     current workspace,
//!   - `n` to open a sub-picker that creates a new surface and seeds it with
//!     `claude` or `codex`,
//!   - Esc to cancel with no side effects.
//!
//! The modal owns *no* I/O — it is a pure state machine. The TUI event loop
//! reads the user's selection out of the modal, runs the appropriate cmux
//! commands, and on success updates `goals.json` via `GoalsFile::set_assignment`.

use crate::cmux::client::SurfaceInfo;
use crate::mc_data::surface_kind::SurfaceKind;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

// ──────────────────────────────────────────────────────────────────────────────
// Data
// ──────────────────────────────────────────────────────────────────────────────

/// A single option in the surface picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOption {
    /// Dispatch to an existing surface that's already running in cmux.
    Existing {
        surface_ref: String,
        kind: SurfaceKind,
        label: String,
    },
    /// Spawn a brand-new surface and seed it with an agent binary.
    NewSurface,
}

/// Which sub-view of the modal is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchView {
    /// The initial view — pick an existing surface, or hit `n` for new.
    PickSurface { selection: usize },
    /// After the user pressed `n` — pick which agent to seed the new surface
    /// with. Today the only options are Claude and Codex.
    PickAgent { selection: usize },
}

/// Outcome of a key press the parent loop must act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Modal absorbed the key; no further action required.
    Handled,
    /// User cancelled. Parent should close the modal with no side effects.
    Cancel,
    /// User selected an existing surface. Parent should `cmux send` to it,
    /// then `GoalsFile::set_assignment(text, surface_ref, kind, now)`.
    SelectExisting {
        surface_ref: String,
        kind: SurfaceKind,
    },
    /// User picked "new surface" with a specific agent kind. Parent should:
    ///   1. `cmux new-surface --type terminal --workspace <ws-ref>` → new_ref
    ///   2. wait ~800ms, `cmux send --surface <new_ref> "<agent>\r"`
    ///   3. wait ~1500ms, `cmux send --surface <new_ref> "<goal text>\r"`
    ///   4. `GoalsFile::set_assignment(text, new_ref, kind, now)`
    NewSurface { kind: SurfaceKind },
}

/// The dispatch modal state. Owned by the WorkspaceState while the modal is
/// open; cleared on Cancel / SelectExisting / NewSurface (after the cmux
/// commands resolve).
#[derive(Debug, Clone)]
pub struct DispatchModal {
    pub goal_text: String,
    /// Workspace UUID — included for downstream tooling (goals.json key,
    /// telemetry); not directly read by the modal itself.
    #[allow(dead_code)]
    pub workspace_uuid: String,
    pub workspace_ref: String,
    pub view: DispatchView,
    pub options: Vec<DispatchOption>,
    /// Set when a cmux call fails — displayed in the status bar before the
    /// modal closes. The parent loop reads it then clears the modal.
    pub error: Option<String>,
}

impl DispatchModal {
    /// Build a modal for `goal_text`. Surfaces are filtered to those whose
    /// kind reads as terminal-style — anything but `OtherAgent` (which we
    /// don't have a glyph distinction for in v1 anyway, but still allow). We
    /// keep at most 9 entries so the 1..=9 numeric shortcut grid stays clean.
    pub fn new(
        goal_text: String,
        workspace_uuid: String,
        workspace_ref: String,
        surfaces: &[SurfaceInfo],
    ) -> Self {
        let mut options: Vec<DispatchOption> = Vec::new();
        for s in surfaces.iter().take(9) {
            // cmux only allows `send` to terminal surfaces; browsers will
            // error out. There is no first-class "is_terminal" flag yet, so
            // we use `tty.is_some()` as the proxy (browser surfaces have
            // tty=None in the tree JSON).
            if s.tty.as_deref().map(|t| !t.is_empty()).unwrap_or(false) {
                options.push(DispatchOption::Existing {
                    surface_ref: s.ref_id.clone(),
                    kind: s.kind,
                    label: s.title.clone(),
                });
            }
        }
        options.push(DispatchOption::NewSurface);
        Self {
            goal_text,
            workspace_uuid,
            workspace_ref,
            view: DispatchView::PickSurface { selection: 0 },
            options,
            error: None,
        }
    }

    /// Convenience for tests: number of selectable options in the current view.
    #[cfg(test)]
    pub fn existing_count(&self) -> usize {
        self.options
            .iter()
            .filter(|o| matches!(o, DispatchOption::Existing { .. }))
            .count()
    }

    /// Handle a key press while the modal is open.
    pub fn handle_key(&mut self, key: KeyEvent) -> DispatchOutcome {
        // Clear stale errors on any user input — they were one-shot status.
        self.error = None;

        match &self.view {
            DispatchView::PickSurface { .. } => self.handle_pick_surface(key),
            DispatchView::PickAgent { .. } => self.handle_pick_agent(key),
        }
    }

    fn handle_pick_surface(&mut self, key: KeyEvent) -> DispatchOutcome {
        match key.code {
            KeyCode::Esc => DispatchOutcome::Cancel,
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.view = DispatchView::PickAgent { selection: 0 };
                DispatchOutcome::Handled
            }
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as u8 - b'1') as usize;
                self.select_existing(idx)
            }
            _ => DispatchOutcome::Handled,
        }
    }

    fn handle_pick_agent(&mut self, key: KeyEvent) -> DispatchOutcome {
        match key.code {
            KeyCode::Esc => {
                // Back up to PickSurface — Esc inside the sub-picker doesn't
                // cancel the whole modal.
                self.view = DispatchView::PickSurface { selection: 0 };
                DispatchOutcome::Handled
            }
            KeyCode::Char('c') | KeyCode::Char('C') => DispatchOutcome::NewSurface {
                kind: SurfaceKind::Claude,
            },
            KeyCode::Char('x') | KeyCode::Char('X') => DispatchOutcome::NewSurface {
                kind: SurfaceKind::Codex,
            },
            _ => DispatchOutcome::Handled,
        }
    }

    fn select_existing(&self, idx: usize) -> DispatchOutcome {
        // Numeric shortcuts index into the `Existing` options *only*; the
        // `NewSurface` sentinel is reached via `n`.
        let existing: Vec<&DispatchOption> = self
            .options
            .iter()
            .filter(|o| matches!(o, DispatchOption::Existing { .. }))
            .collect();
        match existing.get(idx) {
            Some(DispatchOption::Existing {
                surface_ref, kind, ..
            }) => DispatchOutcome::SelectExisting {
                surface_ref: surface_ref.clone(),
                kind: *kind,
            },
            _ => DispatchOutcome::Handled,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Rendering
// ──────────────────────────────────────────────────────────────────────────────

/// Render the modal in the bottom portion of `area`. The modal sits on top of
/// the trajectory view but doesn't replace it — the trajectory is still
/// visible above. We clear the modal's own rect before drawing so border
/// characters don't bleed through.
pub fn render(f: &mut Frame, area: Rect, modal: &DispatchModal) {
    let modal_height = modal_height_for(modal);
    if area.height <= modal_height {
        return;
    }
    let y = area.y + area.height - modal_height;
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height: modal_height,
    };
    f.render_widget(Clear, rect);

    let title = format!(" Dispatch: \"{}\" ", truncate(&modal.goal_text, 60));
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let lines: Vec<Line> = match &modal.view {
        DispatchView::PickSurface { .. } => render_pick_surface_lines(modal),
        DispatchView::PickAgent { .. } => render_pick_agent_lines(),
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn modal_height_for(modal: &DispatchModal) -> u16 {
    match &modal.view {
        DispatchView::PickSurface { .. } => {
            // border (2) + one line per existing option + 'new surface' + 'cancel' line
            let existing = modal
                .options
                .iter()
                .filter(|o| matches!(o, DispatchOption::Existing { .. }))
                .count()
                .min(9) as u16;
            let body = existing + 2; // +1 for "new", +1 for "Esc cancel"
            body + 2
        }
        DispatchView::PickAgent { .. } => 4, // border (2) + 2 body lines
    }
}

fn render_pick_surface_lines(modal: &DispatchModal) -> Vec<Line<'_>> {
    let mut lines: Vec<Line> = Vec::new();
    let mut idx: usize = 0;
    for opt in &modal.options {
        match opt {
            DispatchOption::Existing {
                surface_ref,
                kind,
                label,
            } => {
                let glyph = kind.glyph();
                let kind_label = kind.label();
                let trimmed_label = truncate(label, 24);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  [{}] ", idx + 1),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::styled(
                        format!("{} ", glyph),
                        Style::default().fg(super::trajectory_view::kind_color(*kind)),
                    ),
                    Span::styled(
                        format!("{:7} ", kind_label),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        format!("{:24} ", trimmed_label),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(surface_ref.clone(), Style::default().fg(Color::DarkGray)),
                ]));
                idx += 1;
            }
            DispatchOption::NewSurface => {
                lines.push(Line::from(vec![
                    Span::styled("  [n] ", Style::default().fg(Color::Green)),
                    Span::styled(
                        "+ new surface (pick agent)",
                        Style::default().fg(Color::Gray),
                    ),
                ]));
            }
        }
    }
    lines.push(Line::from(vec![
        Span::styled("  [Esc] ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "cancel",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    ]));
    if let Some(ref err) = modal.error {
        lines.push(Line::from(Span::styled(
            format!("  error: {}", err),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

fn render_pick_agent_lines() -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("  Pick agent: ", Style::default().fg(Color::White)),
            Span::styled("[c] ", Style::default().fg(Color::Yellow)),
            Span::styled(
                "Claude  ",
                Style::default().fg(super::trajectory_view::kind_color(SurfaceKind::Claude)),
            ),
            Span::styled("[x] ", Style::default().fg(Color::Yellow)),
            Span::styled(
                "Codex  ",
                Style::default().fg(super::trajectory_view::kind_color(SurfaceKind::Codex)),
            ),
        ]),
        Line::from(vec![
            Span::styled("  [Esc] ", Style::default().fg(Color::DarkGray)),
            Span::styled("back", Style::default().fg(Color::DarkGray)),
        ]),
    ]
}

fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        s.to_string()
    } else {
        let mut out: String = chars.into_iter().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_surfaces() -> Vec<SurfaceInfo> {
        vec![
            SurfaceInfo {
                title: "claude".into(),
                ref_id: "surface:92".into(),
                tty: Some("ttys001".into()),
                kind: SurfaceKind::Claude,
            },
            SurfaceInfo {
                title: "codex".into(),
                ref_id: "surface:93".into(),
                tty: Some("ttys002".into()),
                kind: SurfaceKind::Codex,
            },
            SurfaceInfo {
                title: "shell".into(),
                ref_id: "surface:94".into(),
                tty: Some("ttys003".into()),
                kind: SurfaceKind::Shell,
            },
            // Browser-style surface (no tty) — should be filtered out.
            SurfaceInfo {
                title: "https://example.com".into(),
                ref_id: "surface:95".into(),
                tty: None,
                kind: SurfaceKind::Unknown,
            },
        ]
    }

    #[test]
    fn new_filters_browsers_and_includes_new_sentinel() {
        let modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        // 3 terminals (92, 93, 94), browser (95) filtered out.
        assert_eq!(modal.existing_count(), 3);
        // Plus the NewSurface sentinel.
        assert!(matches!(
            modal.options.last(),
            Some(DispatchOption::NewSurface)
        ));
    }

    #[test]
    fn numeric_selection_yields_select_existing_with_right_surface() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        // [1] should be surface:92 / Claude (the first terminal).
        let outcome = modal.handle_key(key(KeyCode::Char('1')));
        assert_eq!(
            outcome,
            DispatchOutcome::SelectExisting {
                surface_ref: "surface:92".into(),
                kind: SurfaceKind::Claude,
            }
        );
    }

    #[test]
    fn numeric_selection_second_option_picks_codex() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        let outcome = modal.handle_key(key(KeyCode::Char('2')));
        assert_eq!(
            outcome,
            DispatchOutcome::SelectExisting {
                surface_ref: "surface:93".into(),
                kind: SurfaceKind::Codex,
            }
        );
    }

    #[test]
    fn esc_cancels_in_pick_surface() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        let outcome = modal.handle_key(key(KeyCode::Esc));
        assert_eq!(outcome, DispatchOutcome::Cancel);
    }

    #[test]
    fn n_transitions_to_pick_agent_then_c_yields_new_claude() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        // Start state: PickSurface.
        assert!(matches!(modal.view, DispatchView::PickSurface { .. }));
        // 'n' transitions to PickAgent.
        let outcome1 = modal.handle_key(key(KeyCode::Char('n')));
        assert_eq!(outcome1, DispatchOutcome::Handled);
        assert!(matches!(modal.view, DispatchView::PickAgent { .. }));
        // 'c' fires NewSurface { Claude }.
        let outcome2 = modal.handle_key(key(KeyCode::Char('c')));
        assert_eq!(
            outcome2,
            DispatchOutcome::NewSurface {
                kind: SurfaceKind::Claude,
            }
        );
    }

    #[test]
    fn pick_agent_x_yields_new_codex() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        modal.handle_key(key(KeyCode::Char('n')));
        let outcome = modal.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            outcome,
            DispatchOutcome::NewSurface {
                kind: SurfaceKind::Codex,
            }
        );
    }

    #[test]
    fn pick_agent_esc_returns_to_pick_surface_without_cancelling() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        modal.handle_key(key(KeyCode::Char('n')));
        let outcome = modal.handle_key(key(KeyCode::Esc));
        assert_eq!(outcome, DispatchOutcome::Handled);
        assert!(matches!(modal.view, DispatchView::PickSurface { .. }));
    }

    #[test]
    fn out_of_range_digit_is_handled_silently() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        // 9 has no matching existing option (only 3 terminals).
        let outcome = modal.handle_key(key(KeyCode::Char('9')));
        assert_eq!(outcome, DispatchOutcome::Handled);
        assert!(matches!(modal.view, DispatchView::PickSurface { .. }));
    }

    #[test]
    fn handle_key_clears_prior_error() {
        let mut modal = DispatchModal::new(
            "sprint-02".into(),
            "uuid-a".into(),
            "workspace:25".into(),
            &make_surfaces(),
        );
        modal.error = Some("boom".into());
        modal.handle_key(key(KeyCode::Char('q')));
        assert!(modal.error.is_none());
    }
}
