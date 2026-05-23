use mission_control::mc_data::trajectory::{TrajectoryDoc, Section, Item};

const SAMPLE: &str = "---
workspace: predinvest
workspace_id: 7f3a-uuid
updated: 2026-05-23T15:42:11
snapshot: 7
---

## Goal
- Build self-improvement-enabled investment agent
- (refined) Composable subsystems

## Current surfaces
- claude · mbp · working · writing CalibratedPlanStrategy tests              <!-- mc:surface:7f3a-sid -->
- shell  · mbp · idle    · $ git log --oneline -10                            <!-- mc:surface:4d8e-sid -->

## Tasks & Progress
- [x] sprint-01 composable foundation shipped
- [ ] sprint-02: CalibratedPlanStrategy tests pass
";

#[test]
fn parse_extracts_frontmatter() {
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    assert_eq!(doc.frontmatter.workspace, Some("predinvest".to_string()));
    assert_eq!(doc.frontmatter.workspace_id, Some("7f3a-uuid".to_string()));
    assert_eq!(doc.frontmatter.snapshot, Some(7));
}

#[test]
fn parse_recognizes_three_sections_in_order() {
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    assert_eq!(doc.sections.len(), 3);
    assert_eq!(doc.sections[0].name, "Goal");
    assert_eq!(doc.sections[1].name, "Current surfaces");
    assert_eq!(doc.sections[2].name, "Tasks & Progress");
}

#[test]
fn goal_items_are_plain_bullets() {
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    let goal = &doc.sections[0];
    assert_eq!(goal.items.len(), 2);
    assert_eq!(goal.items[0].text, "Build self-improvement-enabled investment agent");
    assert!(!goal.items[0].is_checkbox);
}

#[test]
fn current_surfaces_extract_surface_id_from_html_comment() {
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    let surfaces = &doc.sections[1];
    assert_eq!(surfaces.items.len(), 2);
    assert_eq!(surfaces.items[0].surface_id.as_deref(), Some("7f3a-sid"));
    assert_eq!(surfaces.items[1].surface_id.as_deref(), Some("4d8e-sid"));
    // The displayed text should NOT include the comment.
    assert!(!surfaces.items[0].text.contains("mc:surface"));
}

#[test]
fn tasks_section_parses_checkbox_state() {
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    let tasks = &doc.sections[2];
    assert_eq!(tasks.items.len(), 2);
    assert!(tasks.items[0].is_checkbox);
    assert_eq!(tasks.items[0].checked, Some(true));
    assert_eq!(tasks.items[1].checked, Some(false));
    assert_eq!(tasks.items[0].text, "sprint-01 composable foundation shipped");
    assert_eq!(tasks.items[1].text, "sprint-02: CalibratedPlanStrategy tests pass");
}

#[test]
fn parse_missing_section_returns_empty_section() {
    let minimal = "---\nworkspace: x\n---\n\n## Goal\n- one bullet\n";
    let doc = TrajectoryDoc::parse(minimal).unwrap();
    let surfaces = doc.section("Current surfaces");
    assert!(surfaces.is_none()); // not present in source

    // Helper: ensure_sections() backfills empties.
    let mut filled = doc.clone();
    filled.ensure_sections();
    assert_eq!(filled.sections.len(), 3);
    assert_eq!(filled.sections[1].name, "Current surfaces");
    assert!(filled.sections[1].items.is_empty());
}
