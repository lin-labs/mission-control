use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use mission_control::mc_data::surface_kind::SurfaceKind;
use mission_control::mc_data::updates::tail::{AgentKind, Turn, read_new};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/updates")
        .join(name)
}

fn fallback() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn tmp_copy(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let p = dir.path().join(name);
    std::fs::copy(fixture(name), &p).unwrap();
    p
}

fn append(p: &Path, s: &str) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(p)
        .unwrap()
        .write_all(s.as_bytes())
        .unwrap();
}

const HUMAN: &str = r#"{"type":"user","message":{"content":"more"},"timestamp":"2026-11-03T00:00:00Z","origin":{"kind":"human"}}"#;

#[test]
fn classifies_origin_kinds() {
    let out = read_new(
        SurfaceKind::Claude,
        &fixture("claude_mixed.jsonl"),
        0,
        fallback(),
    )
    .unwrap();
    let users: Vec<_> = out
        .turns
        .iter()
        .filter(|t| matches!(t, Turn::User { .. }))
        .collect();
    assert_eq!(users.len(), 2);
    assert!(matches!(users[0], Turn::User { text, .. } if text == "placeholder human request"));
    assert!(
        matches!(users[1], Turn::User { text, .. } if text == "placeholder array line one\nplaceholder array line two")
    );
    let injected = out
        .turns
        .iter()
        .filter(|t| {
            matches!(
                t,
                Turn::Agent {
                    kind: AgentKind::Injected,
                    ..
                }
            )
        })
        .count();
    assert_eq!(injected, 4); // peer, notification, letter, no-origin system-reminder
    let replies: Vec<_> = out
        .turns
        .iter()
        .filter(|t| {
            matches!(
                t,
                Turn::Agent {
                    kind: AgentKind::Reply,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(replies.len(), 1);
    assert!(matches!(replies[0], Turn::Agent { text, .. } if text == "placeholder agent reply"));
    assert_eq!(
        out.lines.len(),
        9,
        "every parsed line is carried, tool_result included"
    );
}

#[test]
fn reads_only_new_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let p = tmp_copy(&dir, "claude_mixed.jsonl");
    let first = read_new(SurfaceKind::Claude, &p, 0, fallback()).unwrap();
    assert_eq!(first.next_offset, std::fs::metadata(&p).unwrap().len());
    append(&p, &format!("{HUMAN}\n{HUMAN}\n"));
    let second = read_new(SurfaceKind::Claude, &p, first.next_offset, fallback()).unwrap();
    assert_eq!(second.lines.len(), 2);
    assert_eq!(second.turns.len(), 2);
    assert!(!second.reset);
}

#[test]
fn partial_last_line_is_left_for_next_tick() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, format!("{HUMAN}\n{}", &HUMAN[..40])).unwrap();
    let first = read_new(SurfaceKind::Claude, &p, 0, fallback()).unwrap();
    assert_eq!(first.turns.len(), 1);
    assert_eq!(first.next_offset, HUMAN.len() as u64 + 1);
    append(&p, &format!("{}\n", &HUMAN[40..]));
    let second = read_new(SurfaceKind::Claude, &p, first.next_offset, fallback()).unwrap();
    assert_eq!(second.turns.len(), 1);
    let third = read_new(SurfaceKind::Claude, &p, second.next_offset, fallback()).unwrap();
    assert!(third.turns.is_empty());
}

#[test]
fn truncated_file_resets() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, format!("{HUMAN}\n")).unwrap();
    let out = read_new(SurfaceKind::Claude, &p, 10_000, fallback()).unwrap();
    assert!(out.reset);
    assert_eq!(out.turns.len(), 1);
    assert_eq!(out.next_offset, HUMAN.len() as u64 + 1);
}

#[test]
fn missing_timestamp_uses_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    let no_ts = r#"{"type":"user","message":{"content":"a"},"origin":{"kind":"human"}}"#;
    let bad_ts = r#"{"type":"user","message":{"content":"b"},"timestamp":"not a time","origin":{"kind":"human"}}"#;
    std::fs::write(&p, format!("{no_ts}\nnot json\n{bad_ts}\n{HUMAN}\n")).unwrap();
    let out = read_new(SurfaceKind::Claude, &p, 0, fallback()).unwrap();
    let got: Vec<_> = out
        .turns
        .iter()
        .map(|t| match t {
            Turn::User { at, text } => (text.clone(), *at),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(got[0], ("a".into(), fallback()));
    assert_eq!(got[1], ("b".into(), fallback()));
    assert_eq!(got[2].0, "more");
    assert_ne!(got[2].1, fallback());
    assert_eq!(out.next_offset, std::fs::metadata(&p).unwrap().len());
}

#[test]
fn codex_user_message_is_user() {
    let out = read_new(
        SurfaceKind::Codex,
        &fixture("codex_user.jsonl"),
        0,
        fallback(),
    )
    .unwrap();
    assert_eq!(out.turns.len(), 2);
    assert!(
        matches!(&out.turns[0], Turn::User { text, .. } if text == "placeholder codex request")
    );
    assert!(matches!(
        &out.turns[1],
        Turn::Agent {
            kind: AgentKind::Reply,
            ..
        }
    ));
}

#[test]
fn utf8_and_crlf_lines_parse() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    let a = r#"{"type":"user","message":{"content":"héllo 日本語 🎉"},"timestamp":"2026-11-03T00:00:00Z","origin":{"kind":"human"}}"#;
    std::fs::write(&p, format!("{a}\r\n{a}\r\n")).unwrap();
    let out = read_new(SurfaceKind::Claude, &p, 0, fallback()).unwrap();
    assert_eq!(out.next_offset, std::fs::metadata(&p).unwrap().len());
    assert_eq!(out.turns.len(), 2);
    assert!(matches!(&out.turns[0], Turn::User { text, .. } if text == "héllo 日本語 🎉"));
}
