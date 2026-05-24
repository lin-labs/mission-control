use mission_control::mc_data::events::{self, Event, Kind, Source};
use mission_control::mc_data::paths;
use mission_control::mc_data::trajectory::{Item, Section, TrajectoryDoc, SECTION_GOALS};
use mission_control::mc_data::user_intent::{
    UserIntent, apply_to_tasks, load_for_workspace, normalize_text,
};
use std::fs;

fn with_tmp_home<F: FnOnce()>(f: F) {
    let tmp = tempfile::tempdir().unwrap();
    let prior = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", tmp.path());
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match prior {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn append(uuid: &str, source: Source, kind: Kind, before: Option<&str>, after: Option<&str>) {
    let mut ev = Event::new_now(source, kind, SECTION_GOALS);
    if let Some(b) = before {
        ev = ev.with_before(b);
    }
    if let Some(a) = after {
        ev = ev.with_after(a);
    }
    fs::create_dir_all(paths::workspace_dir(uuid)).unwrap();
    events::append(&paths::events_log(uuid), &ev).unwrap();
}

#[test]
fn normalize_strips_priority_and_checkbox_and_lowercases() {
    assert_eq!(normalize_text("[P0] Build the foo"), "build the foo");
    assert_eq!(normalize_text("[x] [P2] Ship it"), "ship it");
    assert_eq!(normalize_text("  done thing  "), "done thing");
}

#[test]
fn user_intent_extracts_human_check_events_only() {
    with_tmp_home(|| {
        let uuid = "ws-A";
        append(uuid, Source::User, Kind::Check, None, Some("- [x] foo"));
        append(uuid, Source::Agent, Kind::Check, None, Some("- [x] bar")); // ignored
        let intent = load_for_workspace(uuid).unwrap();
        assert!(intent.human_checked.contains(&normalize_text("- [x] foo")));
        assert!(!intent.human_checked.contains(&normalize_text("- [x] bar")));
    });
}

#[test]
fn latest_human_action_wins_uncheck_replaces_check() {
    with_tmp_home(|| {
        let uuid = "ws-B";
        append(uuid, Source::User, Kind::Check, None, Some("foo"));
        append(uuid, Source::User, Kind::Uncheck, None, Some("foo"));
        let intent = load_for_workspace(uuid).unwrap();
        assert!(!intent.human_checked.contains("foo"));
        assert!(intent.human_unchecked.contains("foo"));
    });
}

#[test]
fn apply_to_tasks_force_checks_human_checked_items() {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    doc.sections[2].items.push(Item {
        text: "do the thing".to_string(),
        is_checkbox: true,
        checked: Some(false),
        surface_id: None,
    });
    let mut intent = UserIntent::default();
    intent.human_checked.insert("do the thing".into());
    apply_to_tasks(&mut doc, &intent);
    assert_eq!(doc.sections[2].items[0].checked, Some(true));
}

#[test]
fn apply_to_tasks_drops_human_deleted_items() {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    doc.sections[2].items.push(Item {
        text: "ghost task".to_string(),
        is_checkbox: true,
        checked: Some(false),
        surface_id: None,
    });
    doc.sections[2].items.push(Item {
        text: "keeper".to_string(),
        is_checkbox: true,
        checked: Some(false),
        surface_id: None,
    });
    let mut intent = UserIntent::default();
    intent.human_deleted.insert("ghost task".into());
    apply_to_tasks(&mut doc, &intent);
    assert_eq!(doc.sections[2].items.len(), 1);
    assert_eq!(doc.sections[2].items[0].text, "keeper");
}

#[test]
fn apply_to_tasks_matches_after_priority_prefix_strip() {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    doc.sections[2].items.push(Item {
        text: "[P0] do the thing".to_string(),
        is_checkbox: true,
        checked: Some(false),
        surface_id: None,
    });
    let mut intent = UserIntent::default();
    // intent recorded under normalized form (no prefix)
    intent.human_checked.insert("do the thing".into());
    apply_to_tasks(&mut doc, &intent);
    assert_eq!(doc.sections[2].items[0].checked, Some(true));
}

#[test]
fn agent_actions_dont_count_as_human_intent() {
    with_tmp_home(|| {
        let uuid = "ws-C";
        // Agent checks something, then unchecks. Nothing should stick.
        append(uuid, Source::Agent, Kind::Check, None, Some("foo"));
        append(uuid, Source::Agent, Kind::Delete, Some("bar"), None);
        let intent = load_for_workspace(uuid).unwrap();
        assert!(intent.human_checked.is_empty());
        assert!(intent.human_deleted.is_empty());
    });
}

#[test]
fn human_delete_overrides_earlier_check() {
    with_tmp_home(|| {
        let uuid = "ws-D";
        append(uuid, Source::User, Kind::Check, None, Some("foo"));
        append(uuid, Source::User, Kind::Delete, Some("foo"), None);
        let intent = load_for_workspace(uuid).unwrap();
        assert!(intent.human_deleted.contains("foo"));
        assert!(!intent.human_checked.contains("foo"));
    });
}
