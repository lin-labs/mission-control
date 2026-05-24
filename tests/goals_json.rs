use chrono::{TimeZone, Utc};
use mission_control::mc_data::goals_json::{self, GoalsFile, SurfaceKind, normalize_text};

// Run with --test-threads=1 to avoid concurrent HOME mutations.

fn with_tmp_home<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", tmp.path()) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp.path())));
    match prior {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn normalize_text_examples() {
    assert_eq!(
        normalize_text("  Sprint-02: tests pass."),
        "sprint-02: tests pass"
    );
    assert_eq!(
        normalize_text("sprint-02:\n\ttests pass\t"),
        "sprint-02: tests pass"
    );
    assert_eq!(normalize_text(""), "");
    assert_eq!(normalize_text("???"), "");
}

#[test]
fn load_missing_returns_default_without_error() {
    with_tmp_home(|_| {
        let f = GoalsFile::load("nonexistent-uuid-xyz");
        assert_eq!(f.version, 1);
        assert!(f.goals.is_empty());
    });
}

#[test]
fn round_trip_preserves_struct() {
    with_tmp_home(|_| {
        let uuid = "round-trip-uuid";
        let mut f = GoalsFile::default();
        let ts = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
        f.set_assignment(
            "Sprint-02: tests pass",
            "surf-ref-a",
            SurfaceKind::Claude,
            ts,
        );
        f.set_assignment("write docs", "surf-ref-b", SurfaceKind::Codex, ts);
        f.save(uuid).expect("save");

        let loaded = GoalsFile::load(uuid);
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.goals.len(), 2);
        assert_eq!(loaded.goals[0].text, "Sprint-02: tests pass");
        assert_eq!(loaded.goals[0].text_norm, "sprint-02: tests pass");
        assert_eq!(loaded.goals[0].assigned_surface_ref, "surf-ref-a");
        assert_eq!(loaded.goals[0].assigned_agent_kind, SurfaceKind::Claude);
        assert_eq!(loaded.goals[0].dispatched_at, ts);
        assert!(loaded.goals[0].completed_at.is_none());
        assert_eq!(loaded.goals[1].assigned_agent_kind, SurfaceKind::Codex);
    });
}

#[test]
fn set_assignment_appends_new_entry() {
    let mut f = GoalsFile::default();
    let ts = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
    f.set_assignment("goal one", "ref-1", SurfaceKind::Claude, ts);
    f.set_assignment("goal two", "ref-2", SurfaceKind::Codex, ts);
    assert_eq!(f.goals.len(), 2);
    assert_eq!(f.goals[0].text_norm, "goal one");
    assert_eq!(f.goals[1].text_norm, "goal two");
}

#[test]
fn set_assignment_upserts_existing_text_norm() {
    let mut f = GoalsFile::default();
    let ts1 = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
    let ts2 = Utc.with_ymd_and_hms(2026, 5, 24, 13, 0, 0).unwrap();

    f.set_assignment("Sprint-02: tests pass", "ref-a", SurfaceKind::Claude, ts1);
    // Different spelling/whitespace/case but same text_norm -> should UPDATE, not append.
    f.set_assignment(
        "  sprint-02:  TESTS PASS.  ",
        "ref-b",
        SurfaceKind::Codex,
        ts2,
    );

    assert_eq!(f.goals.len(), 1, "should upsert, not append");
    assert_eq!(f.goals[0].assigned_surface_ref, "ref-b");
    assert_eq!(f.goals[0].assigned_agent_kind, SurfaceKind::Codex);
    assert_eq!(f.goals[0].dispatched_at, ts2);
    assert!(f.goals[0].completed_at.is_none());
}

#[test]
fn upsert_clears_prior_completed_at() {
    let mut f = GoalsFile::default();
    let ts1 = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
    let ts2 = Utc.with_ymd_and_hms(2026, 5, 24, 13, 0, 0).unwrap();
    let ts3 = Utc.with_ymd_and_hms(2026, 5, 24, 14, 0, 0).unwrap();

    f.set_assignment("redo goal", "ref-a", SurfaceKind::Claude, ts1);
    f.complete("redo goal", ts2);
    assert!(f.goals[0].completed_at.is_some());

    // Re-dispatch: same text_norm, should reopen.
    f.set_assignment("redo goal", "ref-c", SurfaceKind::OtherAgent, ts3);
    assert_eq!(f.goals.len(), 1);
    assert!(f.goals[0].completed_at.is_none());
    assert_eq!(f.goals[0].assigned_surface_ref, "ref-c");
}

#[test]
fn complete_sets_completed_at_and_filters_from_opens() {
    let mut f = GoalsFile::default();
    let ts1 = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
    let ts2 = Utc.with_ymd_and_hms(2026, 5, 24, 13, 0, 0).unwrap();
    f.set_assignment("goal one", "surf-x", SurfaceKind::Claude, ts1);

    assert!(f.open_for_goal("goal one").is_some());
    assert_eq!(f.open_for_surface("surf-x").len(), 1);

    f.complete("Goal One.", ts2); // different spelling, same text_norm
    assert_eq!(f.goals[0].completed_at, Some(ts2));
    assert!(f.open_for_goal("goal one").is_none());
    assert!(f.open_for_surface("surf-x").is_empty());
}

#[test]
fn complete_is_noop_when_no_match() {
    let mut f = GoalsFile::default();
    let ts = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
    f.complete("nonexistent goal", ts);
    assert!(f.goals.is_empty());
}

#[test]
fn open_for_surface_filters_by_ref_and_open_status() {
    let mut f = GoalsFile::default();
    let ts = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
    f.set_assignment("a", "ref-1", SurfaceKind::Claude, ts);
    f.set_assignment("b", "ref-1", SurfaceKind::Claude, ts);
    f.set_assignment("c", "ref-2", SurfaceKind::Codex, ts);

    // Fail #1: wrong ref returns empty (filter by ref).
    assert_eq!(f.open_for_surface("ref-2").len(), 1);
    assert_eq!(f.open_for_surface("ref-1").len(), 2);
    assert!(f.open_for_surface("ref-nope").is_empty());

    // Fail #2: completing one removes it from the open set.
    f.complete("a", ts);
    assert_eq!(f.open_for_surface("ref-1").len(), 1);
    assert_eq!(f.open_for_surface("ref-1")[0].text_norm, "b");
}

#[test]
fn load_corrupted_file_returns_default() {
    with_tmp_home(|_| {
        let uuid = "corrupt-uuid";
        let path = goals_json::goals_path(uuid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not valid json").unwrap();

        let f = GoalsFile::load(uuid);
        assert_eq!(f.version, 1);
        assert!(f.goals.is_empty());
    });
}

#[test]
fn save_stamps_version_one_even_if_struct_has_zero() {
    with_tmp_home(|_| {
        let uuid = "ver-stamp-uuid";
        let f = GoalsFile {
            version: 0,
            goals: vec![],
        };
        f.save(uuid).expect("save");
        let raw = std::fs::read_to_string(goals_json::goals_path(uuid)).unwrap();
        assert!(
            raw.contains("\"version\": 1"),
            "version should be stamped to 1, got: {raw}"
        );
    });
}

#[test]
fn save_creates_parent_directory() {
    with_tmp_home(|_| {
        let uuid = "fresh-uuid-no-dir-yet";
        let f = GoalsFile::default();
        f.save(uuid).expect("save should create parent dir");
        assert!(goals_json::goals_path(uuid).exists());
    });
}

#[test]
fn save_does_not_leave_tmp_file_behind() {
    with_tmp_home(|_| {
        let uuid = "atomic-uuid";
        let f = GoalsFile::default();
        f.save(uuid).expect("save");

        let dir = goals_json::goals_path(uuid).parent().unwrap().to_path_buf();
        let mut tmp_count = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".goals.json.tmp.") {
                tmp_count += 1;
            }
        }
        assert_eq!(tmp_count, 0, "no .tmp. leftovers after atomic rename");
    });
}
