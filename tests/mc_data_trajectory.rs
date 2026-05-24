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

#[test]
fn uppercase_checkbox_is_recognized_as_checked() {
    let doc = TrajectoryDoc::parse("## Tasks & Progress\n- [X] done\n").unwrap();
    let item = &doc.sections[0].items[0];
    assert!(item.is_checkbox);
    assert_eq!(item.checked, Some(true));
    assert_eq!(item.text, "done");
}

#[test]
fn ensure_sections_reorders_to_canonical_order() {
    let doc_str = "## Tasks & Progress\n- [x] done\n\n## Goal\n- thing\n";
    let mut doc = TrajectoryDoc::parse(doc_str).unwrap();
    doc.ensure_sections();
    assert_eq!(doc.sections[0].name, "Goal");
    assert_eq!(doc.sections[1].name, "Current surfaces");
    assert_eq!(doc.sections[2].name, "Tasks & Progress");
    // Items preserved through reorder.
    assert_eq!(doc.sections[0].items[0].text, "thing");
    assert_eq!(doc.sections[2].items[0].text, "done");
}

#[test]
fn malformed_surface_comment_yields_none_surface_id() {
    let src = "## Current surfaces\n- claude · mbp · working · stuff <!-- mc:surface:no-closer\n";
    let doc = TrajectoryDoc::parse(src).unwrap();
    let item = &doc.sections[0].items[0];
    assert!(item.surface_id.is_none(), "missing --> must not produce a garbage surface id");
}

#[test]
fn write_then_parse_round_trips() {
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    let serialized = doc.to_markdown();
    let reparsed = TrajectoryDoc::parse(&serialized).unwrap();

    // Frontmatter survives round-trip.
    assert_eq!(reparsed.frontmatter.workspace, doc.frontmatter.workspace);
    assert_eq!(reparsed.frontmatter.workspace_id, doc.frontmatter.workspace_id);
    assert_eq!(reparsed.frontmatter.snapshot, doc.frontmatter.snapshot);

    assert_eq!(
        doc.section("Goal").unwrap().items.len(),
        reparsed.section("Goal").unwrap().items.len()
    );

    // Surface items: ID and visible text both survive.
    let orig_surface = &doc.section("Current surfaces").unwrap().items[0];
    let rep_surface = &reparsed.section("Current surfaces").unwrap().items[0];
    assert_eq!(orig_surface.surface_id, rep_surface.surface_id);
    assert_eq!(orig_surface.text, rep_surface.text);

    let original_task = &doc.section("Tasks & Progress").unwrap().items[0];
    let rep_task = &reparsed.section("Tasks & Progress").unwrap().items[0];
    assert_eq!(original_task.text, rep_task.text);
    assert_eq!(original_task.checked, rep_task.checked);
}

#[test]
fn save_and_load_roundtrip_via_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("trajectory.md");
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    doc.save_to_file(&path).unwrap();
    let loaded = TrajectoryDoc::load_from_file(&path).unwrap();
    assert_eq!(loaded.sections.len(), 3);
    assert_eq!(loaded.frontmatter.snapshot, Some(7));
}

#[test]
fn load_from_missing_file_returns_default_doc() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("does-not-exist.md");
    let loaded = TrajectoryDoc::load_from_file(&path).unwrap();
    // Default = empty frontmatter, canonical empty sections after ensure_sections.
    assert!(loaded.frontmatter.workspace.is_none());
    assert_eq!(loaded.sections.len(), 3); // canonical sections backfilled
    for s in &loaded.sections {
        assert!(s.items.is_empty());
    }
}

#[test]
fn skeleton_has_all_three_canonical_sections_with_frontmatter() {
    let doc = TrajectoryDoc::skeleton("uuid-abc", "predinvest", "predinvest");
    assert_eq!(doc.frontmatter.workspace.as_deref(), Some("predinvest"));
    assert_eq!(doc.frontmatter.workspace_id.as_deref(), Some("uuid-abc"));
    assert_eq!(doc.frontmatter.snapshot, Some(0));
    assert!(doc.section("Goal").is_some());
    assert!(doc.section("Current surfaces").is_some());
    assert!(doc.section("Tasks & Progress").is_some());
}
