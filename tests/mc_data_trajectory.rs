use mission_control::mc_data::trajectory::{TrajectoryDoc, Section, Item, priority_of};

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

#[test]
fn replace_section_items_swaps_in_new_items() {
    let mut doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    let new_items = vec![
        Item {
            text: "claude · mbp · working · writing tests".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("sid-1".to_string()),
        },
        Item {
            text: "shell · mbp · idle · $ ls".to_string(),
            is_checkbox: false,
            checked: None,
            surface_id: Some("sid-2".to_string()),
        },
    ];
    doc.replace_section_items("Current surfaces", new_items);
    let sec = doc.section("Current surfaces").unwrap();
    assert_eq!(sec.items.len(), 2);
    assert_eq!(sec.items[0].surface_id.as_deref(), Some("sid-1"));
    assert_eq!(sec.items[1].surface_id.as_deref(), Some("sid-2"));
}

#[test]
fn replace_section_items_is_noop_for_unknown_section() {
    let mut doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    let before_len = doc.sections.len();
    doc.replace_section_items("Nonexistent", vec![]);
    assert_eq!(doc.sections.len(), before_len, "should not add a new section");
}

#[test]
fn priority_of_extracts_p0_through_p9_prefix() {
    assert_eq!(priority_of("[P0] urgent"), Some(0));
    assert_eq!(priority_of("[P9] later"), Some(9));
    assert_eq!(priority_of("[p3] lowercase"), Some(3));
}

#[test]
fn priority_of_none_when_no_prefix() {
    assert_eq!(priority_of("regular task"), None);
    assert_eq!(priority_of("[Q0] wrong letter"), None);
    assert_eq!(priority_of("[P] no digit"), None);
    assert_eq!(priority_of("[P10] too many"), None);
}

#[test]
fn sort_tasks_noop_when_section_has_10_or_fewer() {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    for i in 0..10 {
        doc.sections[2].items.push(Item {
            text: format!("task {i}"),
            is_checkbox: true,
            checked: Some(i % 2 == 0),
            surface_id: None,
        });
    }
    let before: Vec<String> = doc.sections[2].items.iter().map(|i| i.text.clone()).collect();
    doc.sort_tasks_if_long();
    let after: Vec<String> = doc.sections[2].items.iter().map(|i| i.text.clone()).collect();
    assert_eq!(before, after, "must not sort when <= 10 items");
}

#[test]
fn sort_tasks_with_11_items_splits_todo_first_done_last() {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    for i in 0..11 {
        doc.sections[2].items.push(Item {
            text: format!("task {i}"),
            is_checkbox: true,
            checked: Some(i % 3 == 0),
            surface_id: None,
        });
    }
    doc.sort_tasks_if_long();
    let items = &doc.sections[2].items;
    let mut seen_done = false;
    for it in items {
        let is_done = it.checked == Some(true);
        if is_done {
            seen_done = true;
        } else {
            assert!(!seen_done, "TODO appeared after a DONE: {}", it.text);
        }
    }
}

#[test]
fn sort_tasks_orders_todos_by_priority_high_to_low() {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    let inputs = [
        "no prio task A",
        "[P2] med",
        "[P0] urgent",
        "another no-prio",
        "[P1] medium-high",
        "[P0] another urgent",
        "[P5] later",
        "[P3] meh",
        "[P9] sometime",
        "[P1] also med-high",
        "[P0] third urgent",
    ];
    for t in inputs {
        doc.sections[2].items.push(Item {
            text: t.to_string(),
            is_checkbox: true,
            checked: Some(false),
            surface_id: None,
        });
    }
    doc.sort_tasks_if_long();
    let order: Vec<String> = doc.sections[2].items.iter().map(|i| i.text.clone()).collect();
    // First three should be P0 (in their original insertion order -- stable).
    assert!(order[0].starts_with("[P0] urgent"));
    assert!(order[1].starts_with("[P0] another urgent"));
    assert!(order[2].starts_with("[P0] third urgent"));
    // Next should be P1.
    assert!(order[3].starts_with("[P1] medium-high"));
    assert!(order[4].starts_with("[P1] also med-high"));
    assert!(order[5].starts_with("[P2] med"));
    assert!(order[6].starts_with("[P3] meh"));
    assert!(order[7].starts_with("[P5] later"));
    assert!(order[8].starts_with("[P9] sometime"));
    // Last two are the no-priority items, in original insertion order.
    assert_eq!(order[9], "no prio task A");
    assert_eq!(order[10], "another no-prio");
}

#[test]
fn sort_tasks_preserves_done_insertion_order() {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    let dones = ["d1", "d2", "d3", "d4", "d5", "d6"];
    let todos = ["t1", "t2", "t3", "t4", "t5", "t6"];
    for (i, name) in todos.iter().chain(dones.iter()).enumerate() {
        let is_done = i >= 6;
        doc.sections[2].items.push(Item {
            text: name.to_string(),
            is_checkbox: true,
            checked: Some(is_done),
            surface_id: None,
        });
    }
    doc.sort_tasks_if_long();
    // After sort: all 6 todos first (no priority, stable order = original order),
    // then 6 dones in original order.
    let order: Vec<String> = doc.sections[2].items.iter().map(|i| i.text.clone()).collect();
    assert_eq!(&order[..6], &["t1", "t2", "t3", "t4", "t5", "t6"]);
    assert_eq!(&order[6..], &["d1", "d2", "d3", "d4", "d5", "d6"]);
}
