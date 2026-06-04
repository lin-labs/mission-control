//! Pure formatting helpers for surface rows and goal-row badges in
//! `trajectory.md`. Kept separate from the data model so that the projection
//! pass in `tui::app` and the TUI render path in `tui::trajectory_view` agree
//! on a single source of truth, and so integration tests can exercise the
//! formatting without spinning up the TUI.

use crate::mc_data::goals_json::{GoalEntry, GoalsFile};
use crate::mc_data::surface_kind::SurfaceKind;

/// Maximum length of the goal short-text shown in a `← goal:<short>` badge
/// on a surface row.
pub const SURFACE_GOAL_SHORT_LEN: usize = 30;

/// Two-space gap before a row badge so the markdown still reads cleanly.
const BADGE_GAP: &str = "   ";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceIntentSummary {
    pub overall_goal: Option<String>,
    pub latest_ask: Option<String>,
}

/// Build the rendered `text` field for a surface row in
/// `## Current surfaces`.
///
/// Shape:
///
/// ```text
/// {glyph} {kind_label} · {title}   ← goal:<short>
/// ```
///
/// `← goal:<short>` is appended only when `goals.open_for_surface(surface_ref)`
/// returns at least one entry. When more than one open goal targets this
/// surface the badge becomes `← goal:<N goals>` instead.
pub fn format_surface_text(
    effective_kind: SurfaceKind,
    title: &str,
    goals: &GoalsFile,
    surface_ref: &str,
    intent: Option<&SurfaceIntentSummary>,
) -> String {
    let mut out = String::new();
    out.push(effective_kind.glyph());
    out.push(' ');
    out.push_str(effective_kind.label());
    out.push_str(" · ");
    out.push_str(title);

    let open = goals.open_for_surface(surface_ref);
    if !open.is_empty() {
        out.push_str(BADGE_GAP);
        out.push_str("← goal:");
        if open.len() == 1 {
            out.push_str(&short_goal_text(&open[0].text));
        } else {
            out.push_str(&format!("<{} goals>", open.len()));
        }
    }
    if let Some(intent) = intent {
        if let Some(goal) = intent
            .overall_goal
            .as_deref()
            .map(compact_label)
            .filter(|s| !s.is_empty())
        {
            out.push_str(BADGE_GAP);
            out.push_str("overall:");
            out.push_str(&goal);
        }
        if let Some(ask) = intent
            .latest_ask
            .as_deref()
            .map(compact_label)
            .filter(|s| !s.is_empty())
        {
            out.push_str(BADGE_GAP);
            out.push_str("ask:");
            out.push_str(&ask);
        }
    }
    out
}

/// If the goal with text `text` is currently assigned to a surface, return
/// the badge suffix string `"   → {glyph} {surface_ref}"`. Otherwise None.
///
/// The caller appends this directly to the goal's display text.
pub fn format_goal_badge(goals: &GoalsFile, text: &str) -> Option<String> {
    let entry: &GoalEntry = goals.open_for_goal(text)?;
    let mut out = String::new();
    out.push_str(BADGE_GAP);
    out.push_str("→ ");
    out.push(entry.assigned_agent_kind.glyph());
    out.push(' ');
    out.push_str(&entry.assigned_surface_ref);
    Some(out)
}

/// Strip a previously-appended `   → …` badge or `   ← goal:…` badge from
/// a row's text so the row can be re-rendered idempotently. Returns the
/// trimmed prefix.
///
/// This intentionally splits on the literal three-space `BADGE_GAP` so that
/// user-authored text containing a single `→` or `←` (e.g. inside a goal
/// description) is left alone.
pub fn strip_badge(text: &str) -> &str {
    // Forward-badge for goals: `   → `.
    if let Some(idx) = text.find("   → ") {
        return &text[..idx];
    }
    // Back-badge for surfaces: `   ← goal:`.
    if let Some(idx) = text.find("   ← goal:") {
        return &text[..idx];
    }
    if let Some(idx) = text.find("   overall:") {
        return &text[..idx];
    }
    if let Some(idx) = text.find("   ask:") {
        return &text[..idx];
    }
    text
}

/// Truncate a goal text to at most `SURFACE_GOAL_SHORT_LEN` chars (graphemes
/// approximated by chars; trajectory text is mostly ascii so this is fine).
/// When truncated, the result ends with `…` to make the truncation visible.
fn short_goal_text(text: &str) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= SURFACE_GOAL_SHORT_LEN {
        return trimmed.to_string();
    }
    let mut s: String = chars[..SURFACE_GOAL_SHORT_LEN].iter().collect();
    s.push('…');
    s
}

fn compact_label(text: &str) -> String {
    let cleaned = text
        .replace("   overall:", " overall:")
        .replace("   ask:", " ask:")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&cleaned, 90)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn mk_entry(text: &str, sref: &str, kind: SurfaceKind) -> GoalEntry {
        GoalEntry {
            id: None,
            text: text.to_string(),
            text_norm: crate::mc_data::goals_json::normalize_text(text),
            assigned_surface_ref: sref.to_string(),
            assigned_agent_kind: kind,
            dispatched_at: Utc::now(),
            completed_at: None,
        }
    }

    #[test]
    fn surface_row_includes_glyph_and_label() {
        let goals = GoalsFile::default();
        let s = format_surface_text(
            SurfaceKind::Claude,
            "claude · mbp · working",
            &goals,
            "surface:1",
            None,
        );
        assert!(s.starts_with("✻ claude · "), "got: {s}");
        assert!(s.contains("· working"));
        assert!(!s.contains("← goal:"));
    }

    #[test]
    fn surface_row_appends_single_goal_badge() {
        let mut goals = GoalsFile::default();
        goals.goals.push(mk_entry(
            "Wire up T3 rendering",
            "surface:7",
            SurfaceKind::Codex,
        ));
        let s = format_surface_text(
            SurfaceKind::Codex,
            "codex · mbp · working",
            &goals,
            "surface:7",
            None,
        );
        assert!(s.contains("← goal:Wire up T3 rendering"), "got: {s}");
    }

    #[test]
    fn surface_row_truncates_long_goal_text() {
        let long = "a".repeat(80);
        let mut goals = GoalsFile::default();
        goals
            .goals
            .push(mk_entry(&long, "surface:3", SurfaceKind::Claude));
        let s = format_surface_text(
            SurfaceKind::Claude,
            "claude · mbp · idle",
            &goals,
            "surface:3",
            None,
        );
        // Truncated form ends with `…`.
        assert!(s.contains("← goal:"));
        assert!(s.contains('…'), "expected ellipsis in truncated badge: {s}");
    }

    #[test]
    fn surface_row_with_multiple_goals_shows_count() {
        let mut goals = GoalsFile::default();
        goals
            .goals
            .push(mk_entry("g1", "surface:9", SurfaceKind::Claude));
        goals
            .goals
            .push(mk_entry("g2", "surface:9", SurfaceKind::Claude));
        let s = format_surface_text(
            SurfaceKind::Claude,
            "claude · mbp · working",
            &goals,
            "surface:9",
            None,
        );
        assert!(s.contains("← goal:<2 goals>"), "got: {s}");
    }

    #[test]
    fn surface_row_appends_overall_and_latest_ask() {
        let goals = GoalsFile::default();
        let intent = SurfaceIntentSummary {
            overall_goal: Some("Build the new workspace detail experience".to_string()),
            latest_ask: Some("Replace goals with Beads".to_string()),
        };
        let s = format_surface_text(
            SurfaceKind::Codex,
            "codex · mbp · working",
            &goals,
            "surface:9",
            Some(&intent),
        );
        assert!(s.contains("overall:Build the new workspace detail experience"));
        assert!(s.contains("ask:Replace goals with Beads"));
    }

    #[test]
    fn goal_badge_emits_glyph_and_ref() {
        let mut goals = GoalsFile::default();
        goals.goals.push(mk_entry(
            "Ship surface peek",
            "surface:42",
            SurfaceKind::Claude,
        ));
        let b = format_goal_badge(&goals, "Ship surface peek")
            .expect("badge expected for assigned goal");
        assert!(b.contains("→ ✻ surface:42"), "got: {b}");
    }

    #[test]
    fn goal_badge_returns_none_when_unassigned() {
        let goals = GoalsFile::default();
        assert!(format_goal_badge(&goals, "Anything").is_none());
    }

    #[test]
    fn strip_badge_removes_both_forms() {
        assert_eq!(
            strip_badge("Ship surface peek   → ✻ surface:42"),
            "Ship surface peek"
        );
        assert_eq!(
            strip_badge("✻ claude · mbp · working   ← goal:something"),
            "✻ claude · mbp · working"
        );
        assert_eq!(
            strip_badge("✻ claude · mbp · working   overall:goal   ask:ask"),
            "✻ claude · mbp · working"
        );
        // Idempotent on text without a badge.
        assert_eq!(strip_badge("plain text"), "plain text");
    }

    #[test]
    fn goal_badge_matches_via_text_norm() {
        let mut goals = GoalsFile::default();
        goals.goals.push(mk_entry(
            "  Ship  surface peek!  ",
            "surface:42",
            SurfaceKind::Codex,
        ));
        // Different spelling but same normalized form → still matches.
        let b = format_goal_badge(&goals, "ship SURFACE peek").expect("expected match");
        assert!(b.contains("→ ▲ surface:42"));
    }
}
