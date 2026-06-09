use mission_control::mc_data::mux_state;

fn write_state(dir: &std::path::Path, id: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), body).unwrap();
}

#[test]
fn parses_protocol_fixture_and_zero_times_as_never() {
    let tmp = tempfile::tempdir().unwrap();
    write_state(
        tmp.path(),
        "s-working",
        r#"{
  "session_id": "s-working",
  "agent": "grok",
  "created_at": "2026-06-09T13:35:42-07:00",
  "updated_at": "2026-06-09T13:36:08-07:00",
  "last_event": "tool_start",
  "last_tool": "run_terminal_command",
  "working": true,
  "turn_count": 0,
  "events_seen": 2,
  "last_prompt_submit_at": "2026-06-09T13:36:02-07:00",
  "last_turn_end_at": "0001-01-01T00:00:00Z"
}"#,
    );

    let state = mux_state::load_session_in_dir(tmp.path(), "s-working")
        .unwrap()
        .unwrap();

    assert_eq!(state.session_id, "s-working");
    assert_eq!(state.agent, "grok");
    assert_eq!(state.last_event, "tool_start");
    assert_eq!(state.last_tool.as_deref(), Some("run_terminal_command"));
    assert!(state.working);
    assert_eq!(state.turn_count, 0);
    assert_eq!(state.events_seen, 2);
    assert!(state.last_prompt_submit_at.is_some());
    assert!(state.last_turn_end_at.is_none());
    assert!(!state.has_ended_turn());
}

#[test]
fn loads_archived_session_when_active_doc_is_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let archived = tmp.path().join("archived");
    write_state(
        &archived,
        "s-ended",
        r#"{
  "session_id": "s-ended",
  "agent": "claude",
  "created_at": "2026-06-09T13:35:42-07:00",
  "updated_at": "2026-06-09T13:36:08-07:00",
  "last_event": "turn_end",
  "last_tool": "Write",
  "working": false,
  "turn_count": 1,
  "events_seen": 8,
  "last_prompt_submit_at": "2026-06-09T13:36:02-07:00",
  "last_turn_end_at": "2026-06-09T13:36:08-07:00"
}"#,
    );

    let state = mux_state::load_session_in_dir(tmp.path(), "s-ended")
        .unwrap()
        .unwrap();

    assert_eq!(state.agent, "claude");
    assert!(!state.working);
    assert!(state.has_ended_turn());
}

#[test]
fn load_all_ignores_malformed_and_subdirectories() {
    let tmp = tempfile::tempdir().unwrap();
    write_state(
        tmp.path(),
        "s-good",
        r#"{
  "session_id": "s-good",
  "agent": "claude",
  "created_at": "2026-06-09T13:35:42-07:00",
  "updated_at": "2026-06-09T13:36:08-07:00",
  "last_event": "turn_end",
  "last_tool": null,
  "working": false,
  "turn_count": 1,
  "events_seen": 2,
  "last_prompt_submit_at": "0001-01-01T00:00:00Z",
  "last_turn_end_at": "2026-06-09T13:36:08-07:00"
}"#,
    );
    std::fs::write(tmp.path().join("bad.json"), "{not json").unwrap();
    std::fs::create_dir(tmp.path().join("archived")).unwrap();

    let states = mux_state::load_all_in_dir(tmp.path());

    assert_eq!(states.len(), 1);
    assert_eq!(states[0].session_id, "s-good");
}

#[test]
fn rejects_unsafe_session_ids() {
    let tmp = tempfile::tempdir().unwrap();

    assert!(
        mux_state::load_session_in_dir(tmp.path(), "../escape")
            .unwrap()
            .is_none()
    );
    assert!(
        mux_state::load_session_in_dir(tmp.path(), "nested/id")
            .unwrap()
            .is_none()
    );
}
