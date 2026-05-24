use mission_control::mc_data::session_log::{self, WorkspaceContext};
use std::fs;

const SAMPLE: &str = "---
date: 2026-05-23
workspace_id: uuid-1
status: working
---

# Session — Test

## 17:30 PT — boyan
first ask, with multiple
lines of content
that should all be captured

---

## 17:31 PT — claude
did stuff

---

## 17:42 PT — boyan
second ask
";

#[test]
fn last_user_turn_returns_most_recent() {
    let t = session_log::last_user_turn(SAMPLE).unwrap();
    assert_eq!(t.trim(), "second ask");
}

#[test]
fn parse_extracts_all_turns_in_order() {
    let turns = session_log::parse(SAMPLE);
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].role, "boyan");
    assert!(turns[0].content.contains("first ask"));
    assert!(turns[0].content.contains("multiple"));
    assert_eq!(turns[1].role, "claude");
    assert_eq!(turns[2].role, "boyan");
}

#[test]
fn last_user_turn_returns_none_when_no_user_turn() {
    let s = "---\n---\n\n## 12:00 PT — claude\nself-monologue\n";
    assert!(session_log::last_user_turn(s).is_none());
}

#[test]
fn parse_tolerates_regular_hyphen_in_heading() {
    let s = "## 17:30 PT - boyan\nhello\n";
    let turns = session_log::parse(s);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].role, "boyan");
    assert_eq!(turns[0].content.trim(), "hello");
}

#[test]
fn latest_session_file_picks_most_recent_match() {
    let tmp = tempfile::tempdir().unwrap();
    let obs = tmp.path().join("obs");
    fs::create_dir_all(obs.join("Sessions")).unwrap();
    // SAFETY: tests run --test-threads=1
    let prior = std::env::var_os("OBS_AGENTS");
    unsafe { std::env::set_var("OBS_AGENTS", &obs); }

    let result = std::panic::catch_unwind(|| {
        // Write 2 candidate files + 1 unrelated.
        fs::write(
            obs.join("Sessions/2026-05-23-17-a.md"),
            "---\nworkspace_id: target\n---\n\n## 17:30 PT — boyan\nold\n",
        ).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            obs.join("Sessions/2026-05-23-18-b.md"),
            "---\nworkspace_id: target\n---\n\n## 18:00 PT — boyan\nnew\n",
        ).unwrap();
        fs::write(
            obs.join("Sessions/2026-05-23-19-c.md"),
            "---\nworkspace_id: other\n---\n\n## 19:00 PT — boyan\nunrelated\n",
        ).unwrap();

        // Use empty ctx → tier 1 skipped, falls back to tier 2 (uuid match).
        let ctx = WorkspaceContext::default();
        let picked = session_log::latest_session_file_for_workspace("target", &ctx)
            .unwrap()
            .expect("expected a matching file");
        assert!(picked.ends_with("2026-05-23-18-b.md"), "got {picked:?}");
    });

    match prior {
        Some(v) => unsafe { std::env::set_var("OBS_AGENTS", v) },
        None => unsafe { std::env::remove_var("OBS_AGENTS") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn latest_session_file_returns_none_when_dir_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let prior = std::env::var_os("OBS_AGENTS");
    unsafe { std::env::set_var("OBS_AGENTS", tmp.path().join("does-not-exist")); }
    let result = std::panic::catch_unwind(|| {
        let ctx = WorkspaceContext::default();
        let r = session_log::latest_session_file_for_workspace("any-uuid", &ctx).unwrap();
        assert!(r.is_none());
    });
    match prior {
        Some(v) => unsafe { std::env::set_var("OBS_AGENTS", v) },
        None => unsafe { std::env::remove_var("OBS_AGENTS") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
