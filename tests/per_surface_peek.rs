/// Integration tests for per-surface session log distribution.
///
/// Verifies that `resolve_session_log_for_surface` hands out distinct session
/// logs to surfaces that share the same workspace (same host+cwd tier) by using
/// `surface_index` (index_in_pane) as the distribution key.
use mission_control::mc_data::session_log::{self, WorkspaceContext};
use std::fs;

/// Build a minimal session log with the given workspace_id in frontmatter.
fn make_log(workspace_id: &str, label: &str) -> String {
    format!(
        "---\ndate: 2026-05-24\nworkspace_id: {workspace_id}\nstatus: working\n---\n\n\
## 12:00 PT \u{2014} boyan\n{label}\n"
    )
}

/// Three surfaces in the same workspace get three distinct session logs,
/// distributed newest-first by surface_index.
#[test]
fn three_surfaces_get_distinct_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let obs = tmp.path().join("obs");
    fs::create_dir_all(obs.join("Sessions")).unwrap();

    let workspace_id = "per-surface-peek-test-uuid-three-77777";

    // Write 3 logs with the same workspace_id, spaced to ensure distinct mtimes.
    fs::write(
        obs.join("Sessions/2026-05-24-surface-a.md"),
        make_log(workspace_id, "surface-a-content"),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(
        obs.join("Sessions/2026-05-24-surface-b.md"),
        make_log(workspace_id, "surface-b-content"),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(
        obs.join("Sessions/2026-05-24-surface-c.md"),
        make_log(workspace_id, "surface-c-content"),
    )
    .unwrap();

    let prior_obs = std::env::var_os("OBS_AGENTS");
    // SAFETY: tests run with --test-threads=1
    unsafe { std::env::set_var("OBS_AGENTS", &obs) };

    let result = std::panic::catch_unwind(|| {
        // Empty ctx → tier 1 skipped → tier 2 (uuid match) applies.
        let ctx = WorkspaceContext::default();

        let r0 = session_log::resolve_session_log_for_surface(workspace_id, "surface:92", &ctx, Some("claude"), 0)
            .expect("resolve idx=0 should not error")
            .expect("expected Some for index 0");

        let r1 = session_log::resolve_session_log_for_surface(workspace_id, "surface:93", &ctx, Some("claude"), 1)
            .expect("resolve idx=1 should not error")
            .expect("expected Some for index 1");

        let r2 = session_log::resolve_session_log_for_surface(workspace_id, "surface:94", &ctx, Some("claude"), 2)
            .expect("resolve idx=2 should not error")
            .expect("expected Some for index 2");

        // All three paths must be distinct.
        assert_ne!(r0, r1, "surface 0 and 1 should get different logs");
        assert_ne!(r1, r2, "surface 1 and 2 should get different logs");
        assert_ne!(r0, r2, "surface 0 and 2 should get different logs");

        // Index 0 → newest (surface-c), index 1 → middle (surface-b), index 2 → oldest (surface-a).
        let t0 = fs::read_to_string(&r0).unwrap();
        let t1 = fs::read_to_string(&r1).unwrap();
        let t2 = fs::read_to_string(&r2).unwrap();
        assert!(t0.contains("surface-c-content"), "idx 0 should be newest; got {r0:?}");
        assert!(t1.contains("surface-b-content"), "idx 1 should be middle; got {r1:?}");
        assert!(t2.contains("surface-a-content"), "idx 2 should be oldest; got {r2:?}");
    });

    match prior_obs {
        Some(v) => unsafe { std::env::set_var("OBS_AGENTS", v) },
        None => unsafe { std::env::remove_var("OBS_AGENTS") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// Out-of-range index returns the oldest match (last in mtime-desc order).
#[test]
fn out_of_range_index_returns_oldest_log() {
    let tmp = tempfile::tempdir().unwrap();
    let obs = tmp.path().join("obs");
    fs::create_dir_all(obs.join("Sessions")).unwrap();

    let workspace_id = "per-surface-peek-test-uuid-outofrange-66666";

    fs::write(
        obs.join("Sessions/2026-05-24-oldest.md"),
        make_log(workspace_id, "oldest-content"),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::write(
        obs.join("Sessions/2026-05-24-newest.md"),
        make_log(workspace_id, "newest-content"),
    )
    .unwrap();

    let prior_obs = std::env::var_os("OBS_AGENTS");
    // SAFETY: tests run with --test-threads=1
    unsafe { std::env::set_var("OBS_AGENTS", &obs) };

    let result = std::panic::catch_unwind(|| {
        let ctx = WorkspaceContext::default();

        // Index 5 is beyond the 2 matches → should return the oldest log.
        let r = session_log::resolve_session_log_for_surface(workspace_id, "surface:99", &ctx, Some("claude"), 5)
            .expect("resolve idx=5 should not error")
            .expect("expected Some for out-of-range index");

        let text = fs::read_to_string(&r).unwrap();
        assert!(
            text.contains("oldest-content"),
            "out-of-range index should return oldest log; got {r:?}"
        );
    });

    match prior_obs {
        Some(v) => unsafe { std::env::set_var("OBS_AGENTS", v) },
        None => unsafe { std::env::remove_var("OBS_AGENTS") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
