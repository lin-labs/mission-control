/// Integration tests for the agent-source peek mode.
///
/// Tests cover:
/// - `resolve_session_log_for_surface_in_dir` step 2 (workspace-level fallback).
/// - `resolve_session_log_for_surface` step 3: returns None when no session log exists.
/// - Session log content parsing used as peek buffer input.
///
/// The peek_view rendering helpers (rebuild_agent_buffer, truncate_words,
/// is_user_role) are tested in unit tests inside peek_view.rs.
use mission_control::mc_data::session_log::{self, WorkspaceContext};
use std::fs;

/// Build a minimal session log with the given workspace_id in frontmatter.
fn make_session_log(workspace_id: &str, user_text: &str, assistant_text: &str) -> String {
    format!(
        "---\ndate: 2026-05-23\nworkspace_id: {workspace_id}\nstatus: working\n---\n\n\
## 12:00 PT \u{2014} boyan\n{user_text}\n\n---\n\n\
## 12:01 PT \u{2014} claude\n{assistant_text}\n"
    )
}

// ── resolve_session_log_for_surface ─────────────────────────────────────────

/// Step 2: when no pointer file exists, falls back to latest_session_file_for_workspace.
#[test]
fn resolve_falls_back_to_workspace_log_when_no_pointer() {
    let tmp = tempfile::tempdir().unwrap();
    let histories = tmp.path().join("histories");
    fs::create_dir_all(&histories).unwrap();

    // Use a UUID that certainly has no pointer file in the real data dir.
    let uuid = "peek-agent-test-uuid-fallback-99999";
    let log = make_session_log(uuid, "fallback-question", "fallback-answer");
    fs::write(histories.join("2026-05-23-fallback.md"), &log).unwrap();

    // No pointer file will exist for this UUID (it's a made-up UUID).
    // The resolver should fall through to workspace-level lookup.
    // Empty ctx -> tier 1 skipped -> tier 2 (uuid match) applies.
    let ctx = WorkspaceContext::default();
    let resolved = session_log::resolve_session_log_for_surface_in_dir(
        &histories,
        uuid,
        "sid-no-pointer",
        &ctx,
        Some("claude"),
        0,
    )
    .expect("resolve should not error");
    assert!(
        resolved.is_some(),
        "expected Some from workspace-level fallback"
    );
    let text = fs::read_to_string(resolved.unwrap().path).unwrap();
    assert!(
        text.contains("fallback-question"),
        "session log content mismatch"
    );
}

/// Step 3: when no pointer file AND no workspace session log → returns None (Shell source).
#[test]
fn resolve_returns_none_when_no_log_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let histories = tmp.path().join("histories-empty");

    let ctx = WorkspaceContext::default();
    let resolved = session_log::resolve_session_log_for_surface_in_dir(
        &histories,
        "peek-agent-test-no-log-uuid",
        "sid-none",
        &ctx,
        Some("claude"),
        0,
    )
    .expect("resolve should not error");
    assert!(
        resolved.is_none(),
        "expected None -> Shell source when no session log"
    );
}

// ── Session log parsing used by agent peek buffer ────────────────────────────

/// Parsing a session log returns the turns that rebuild_agent_buffer will format.
/// Verifies that user turn content is verbatim and assistant turn content is
/// returned as-is from parse() (truncation happens in rebuild_agent_buffer,
/// which is tested in peek_view.rs unit tests).
#[test]
fn agent_peek_session_log_parse_returns_correct_turns() {
    let user_text = "implement the full agent-source peek mode";
    let assistant_text = (0..110)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ");

    let log = make_session_log("ws-parse-test", user_text, &assistant_text);
    let turns = session_log::parse(&log);

    assert_eq!(turns.len(), 2, "expected exactly 2 turns");
    assert_eq!(turns[0].role, "boyan");
    assert!(turns[0].content.contains(user_text));
    assert_eq!(turns[1].role, "claude");
    // parse() returns content verbatim — truncation is downstream.
    assert!(turns[1].content.contains("word0"));
    assert!(turns[1].content.contains("word109"));
}

/// Resolve correctly returns the most-recently-modified session log for the
/// workspace when multiple session logs exist for the same workspace UUID.
#[test]
fn resolve_returns_most_recent_session_log() {
    let tmp = tempfile::tempdir().unwrap();
    let histories = tmp.path().join("histories");
    fs::create_dir_all(&histories).unwrap();

    let uuid = "peek-agent-test-uuid-most-recent-88888";
    let log_old = make_session_log(uuid, "old-question", "old-answer");
    let log_new = make_session_log(uuid, "new-question", "new-answer");

    fs::write(histories.join("2026-05-23-old.md"), &log_old).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(histories.join("2026-05-23-new.md"), &log_new).unwrap();

    let ctx = WorkspaceContext::default();
    let resolved = session_log::resolve_session_log_for_surface_in_dir(
        &histories,
        uuid,
        "sid-mr",
        &ctx,
        Some("claude"),
        0,
    )
    .expect("resolve should not error")
    .expect("expected Some from workspace-level fallback")
    .path;
    let text = fs::read_to_string(&resolved).unwrap();
    assert!(
        text.contains("new-question"),
        "should pick the most recent log, got: {resolved:?}"
    );
}
