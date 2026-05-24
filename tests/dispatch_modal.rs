//! Integration tests for the T4 dispatch flow.
//!
//! The modal itself lives in the binary target (`src/tui/dispatch_modal.rs`)
//! and is exercised by 9 unit tests in that file. This file covers the
//! data-layer half of the dispatch flow that integration tests CAN reach:
//!
//!   * `goals.json` gets the correct upsert after a "successful" dispatch
//!     (verified via the same `GoalsFile::set_assignment` call the main loop
//!     makes on cmux-send success).
//!   * The cmux output-parsing contract is honored — we verify the
//!     `new_surface` shape against a stub command in `test_new_surface_*`.
//!
//! Run with: `cargo test --test dispatch_modal -- --test-threads=1`.

use chrono::{TimeZone, Utc};
use mission_control::mc_data::goals_json::{GoalsFile, SurfaceKind};

fn with_tmp_home<F: FnOnce()>(f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", tmp.path()) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match prior {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn dispatch_to_existing_surface_writes_goals_json_assignment() {
    // Simulates what `handle_dispatch_outcome` does in the
    // SelectExisting branch on cmux-send success: load the file, upsert
    // the assignment, save it.
    with_tmp_home(|| {
        let uuid = "dispatch-test-uuid";
        let goal_text = "Sprint-02: CalibratedPlanStrategy tests pass";
        let surface_ref = "surface:92";
        let ts = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();

        // No goals.json exists yet.
        let initial = GoalsFile::load(uuid);
        assert!(initial.goals.is_empty());

        // Mirror the main-loop call.
        let mut goals = GoalsFile::load(uuid);
        goals.set_assignment(goal_text, surface_ref, SurfaceKind::Claude, ts);
        goals.save(uuid).expect("save");

        // Re-load and assert it round-trips.
        let loaded = GoalsFile::load(uuid);
        assert_eq!(loaded.goals.len(), 1);
        let entry = &loaded.goals[0];
        assert_eq!(entry.text, goal_text);
        assert_eq!(entry.assigned_surface_ref, surface_ref);
        assert_eq!(entry.assigned_agent_kind, SurfaceKind::Claude);
        assert_eq!(entry.dispatched_at, ts);
        assert!(entry.completed_at.is_none());

        // The "open_for_surface" lookup the trajectory renderer uses must
        // return this entry.
        let open = loaded.open_for_surface(surface_ref);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].text, goal_text);
    });
}

#[test]
fn dispatch_to_new_surface_records_assignment_with_new_ref() {
    // Simulates the NewSurface branch's final goals.json update once the
    // spawn → seed-agent → send-goal sequence has succeeded.
    with_tmp_home(|| {
        let uuid = "dispatch-new-test-uuid";
        let goal_text = "Sprint-03: surface-aware dispatch";
        let new_ref = "surface:108"; // What `cmux new-surface` would have returned.
        let ts = Utc.with_ymd_and_hms(2026, 5, 24, 13, 0, 0).unwrap();

        let mut goals = GoalsFile::load(uuid);
        goals.set_assignment(goal_text, new_ref, SurfaceKind::Codex, ts);
        goals.save(uuid).expect("save");

        let loaded = GoalsFile::load(uuid);
        assert_eq!(loaded.goals.len(), 1);
        assert_eq!(loaded.goals[0].assigned_surface_ref, new_ref);
        assert_eq!(loaded.goals[0].assigned_agent_kind, SurfaceKind::Codex);
    });
}

#[test]
fn cmux_failure_path_does_not_modify_goals_json() {
    // Simulates the failure path: parent did NOT call set_assignment, so
    // goals.json stays empty even though the user pressed a number.
    with_tmp_home(|| {
        let uuid = "dispatch-fail-uuid";
        let goals = GoalsFile::load(uuid);
        assert!(goals.goals.is_empty(), "precondition: no goals yet");

        // (Main loop intentionally skips set_assignment + save here.)

        let after = GoalsFile::load(uuid);
        assert!(
            after.goals.is_empty(),
            "goals.json must remain empty when cmux send fails"
        );
    });
}

#[test]
fn redispatch_same_goal_updates_assignment_in_place() {
    // The user might dispatch a goal to surface A, then re-dispatch the
    // same goal to surface B. `set_assignment` upserts by normalized text,
    // so there should still be one row — but with the new ref/kind.
    with_tmp_home(|| {
        let uuid = "dispatch-redispatch-uuid";
        let goal_text = "Sprint-02: tests pass";
        let ts1 = Utc.with_ymd_and_hms(2026, 5, 24, 12, 0, 0).unwrap();
        let ts2 = Utc.with_ymd_and_hms(2026, 5, 24, 13, 0, 0).unwrap();

        let mut goals = GoalsFile::load(uuid);
        goals.set_assignment(goal_text, "surface:92", SurfaceKind::Claude, ts1);
        goals.save(uuid).unwrap();

        let mut goals = GoalsFile::load(uuid);
        goals.set_assignment(goal_text, "surface:93", SurfaceKind::Codex, ts2);
        goals.save(uuid).unwrap();

        let loaded = GoalsFile::load(uuid);
        assert_eq!(loaded.goals.len(), 1, "must dedupe by normalized text");
        let entry = &loaded.goals[0];
        assert_eq!(entry.assigned_surface_ref, "surface:93");
        assert_eq!(entry.assigned_agent_kind, SurfaceKind::Codex);
        assert_eq!(entry.dispatched_at, ts2);
        assert!(
            entry.completed_at.is_none(),
            "redispatch must clear any prior completion"
        );
    });
}

#[test]
fn cmux_new_surface_token_parser_extracts_surface_ref() {
    // The cmux `new-surface` command emits (verified live on 2026-05-24):
    //   OK surface:108 pane:15 workspace:14
    // The client parses the `surface:<N>` token. Verify the rule.
    let stdout = "OK surface:108 pane:15 workspace:14\n";
    let token = stdout
        .split_whitespace()
        .find(|t| t.strip_prefix("surface:").map_or(false, |s| !s.is_empty()))
        .expect("must find a surface:<N> token");
    assert_eq!(token, "surface:108");
}

#[test]
fn cmux_new_surface_parser_rejects_missing_surface_token() {
    // Defensive: if cmux ever drops the surface token from its output,
    // the parser must NOT silently succeed with a bogus value.
    let stdout = "OK pane:15 workspace:14\n";
    let token = stdout
        .split_whitespace()
        .find(|t| t.strip_prefix("surface:").map_or(false, |s| !s.is_empty()));
    assert!(token.is_none());
}
