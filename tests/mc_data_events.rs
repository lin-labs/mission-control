use mission_control::mc_data::events::{self, Event, Kind, Source};
use std::path::Path;

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
fn append_and_load_roundtrip() {
    with_tmp_home(|tmp| {
        let path = tmp.join("events.jsonl");
        let ev = Event::new_now(Source::User, Kind::Check, "Tasks & Progress")
            .with_before("- [ ] task")
            .with_after("- [x] task")
            .with_snapshot(1)
            .with_explanation("codex finished");

        events::append(&path, &ev).expect("append");
        let loaded = events::load(&path).expect("load");

        assert_eq!(loaded.len(), 1);
        let got = &loaded[0];
        assert!(matches!(got.source, Source::User));
        assert!(matches!(got.kind, Kind::Check));
        assert_eq!(got.section, "Tasks & Progress");
        assert_eq!(got.before.as_deref(), Some("- [ ] task"));
        assert_eq!(got.after.as_deref(), Some("- [x] task"));
        assert_eq!(got.snapshot, Some(1));
        assert_eq!(got.user_explanation.as_deref(), Some("codex finished"));
    });
}

#[test]
fn multiple_appends_accumulate() {
    with_tmp_home(|tmp| {
        let path = tmp.join("events.jsonl");
        for i in 0..3 {
            let ev = Event::new_now(Source::Agent, Kind::Add, "Tasks & Progress")
                .with_after(format!("- [ ] task-{i}"));
            events::append(&path, &ev).expect("append");
        }
        let loaded = events::load(&path).expect("load");
        assert_eq!(loaded.len(), 3);
        for (i, ev) in loaded.iter().enumerate() {
            assert_eq!(
                ev.after.as_deref(),
                Some(format!("- [ ] task-{i}").as_str())
            );
        }
    });
}

#[test]
fn load_nonexistent_returns_empty() {
    let path = Path::new("/tmp/definitely_does_not_exist_mc_events_test.jsonl");
    let result = events::load(path).expect("load");
    assert!(result.is_empty());
}

#[test]
fn oversized_event_returns_err() {
    with_tmp_home(|tmp| {
        let path = tmp.join("events.jsonl");
        // Create a before string that will push the serialized line over 4096 bytes.
        let big = "x".repeat(4097);
        let ev = Event::new_now(Source::User, Kind::Edit, "Tasks & Progress").with_before(big);
        let result = events::append(&path, &ev);
        assert!(result.is_err(), "expected Err for oversized event");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("byte cap") || msg.contains("bytes exceeds"),
            "error message: {msg}"
        );
    });
}

#[test]
fn malformed_line_is_silently_skipped() {
    with_tmp_home(|tmp| {
        let path = tmp.join("events.jsonl");
        // Write a valid event, then a malformed line, then another valid event.
        let ev1 = Event::new_now(Source::User, Kind::Add, "Goal").with_after("- goal text");
        let ev2 = Event::new_now(Source::User, Kind::Delete, "Goal").with_before("- old goal");

        events::append(&path, &ev1).expect("append ev1");

        // Inject a malformed line directly.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(f, "{{not valid json...").expect("write bad line");

        events::append(&path, &ev2).expect("append ev2");

        let loaded = events::load(&path).expect("load");
        // The malformed line should be silently dropped; we should get 2 events.
        assert_eq!(loaded.len(), 2);
        assert!(matches!(loaded[0].kind, Kind::Add));
        assert!(matches!(loaded[1].kind, Kind::Delete));
    });
}

#[test]
fn source_user_undo_serializes_correctly() {
    with_tmp_home(|tmp| {
        let path = tmp.join("events.jsonl");
        let ev = Event::new_now(Source::UserUndo, Kind::Uncheck, "Tasks & Progress")
            .with_before("- [x] done");
        events::append(&path, &ev).expect("append");

        // Check the raw file for the correct source value.
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(
            raw.contains("\"user-undo\""),
            "source should serialize as 'user-undo', got: {raw}"
        );

        let loaded = events::load(&path).expect("load");
        assert!(matches!(loaded[0].source, Source::UserUndo));
    });
}
