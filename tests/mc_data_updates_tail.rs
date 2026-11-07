use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use mission_control::mc_data::surface_kind::SurfaceKind;
use mission_control::mc_data::updates::tail::{AgentKind, Turn, line_start_at, read_new};

const ALL: u64 = u64::MAX;

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
        ALL,
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
    assert_eq!(injected, 6); // peer, notification, letter, no-origin system-reminder, 2 sdk
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
        11,
        "every parsed line is carried, tool_result included"
    );
}

#[test]
fn sdk_prompts_without_origin_are_injected() {
    let out = read_new(
        SurfaceKind::Claude,
        &fixture("claude_mixed.jsonl"),
        0,
        fallback(),
        ALL,
    )
    .unwrap();
    for text in ["placeholder scheduled prompt", "placeholder sdk run prompt"] {
        assert!(
            out.turns.iter().any(
                |t| matches!(t, Turn::Agent { text: x, kind: AgentKind::Injected, .. } if x == text)
            ),
            "{text}"
        );
    }
}

#[test]
fn reads_only_new_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let p = tmp_copy(&dir, "claude_mixed.jsonl");
    let first = read_new(SurfaceKind::Claude, &p, 0, fallback(), ALL).unwrap();
    assert_eq!(first.next_offset, std::fs::metadata(&p).unwrap().len());
    append(&p, &format!("{HUMAN}\n{HUMAN}\n"));
    let second = read_new(SurfaceKind::Claude, &p, first.next_offset, fallback(), ALL).unwrap();
    assert_eq!(second.lines.len(), 2);
    assert_eq!(second.turns.len(), 2);
    assert!(!second.reset);
}

#[test]
fn partial_last_line_is_left_for_next_tick() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, format!("{HUMAN}\n{}", &HUMAN[..40])).unwrap();
    let first = read_new(SurfaceKind::Claude, &p, 0, fallback(), ALL).unwrap();
    assert_eq!(first.turns.len(), 1);
    assert_eq!(first.next_offset, HUMAN.len() as u64 + 1);
    append(&p, &format!("{}\n", &HUMAN[40..]));
    let second = read_new(SurfaceKind::Claude, &p, first.next_offset, fallback(), ALL).unwrap();
    assert_eq!(second.turns.len(), 1);
    let third = read_new(SurfaceKind::Claude, &p, second.next_offset, fallback(), ALL).unwrap();
    assert!(third.turns.is_empty());
}

#[test]
fn truncated_file_resets() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, format!("{HUMAN}\n")).unwrap();
    let out = read_new(SurfaceKind::Claude, &p, 10_000, fallback(), ALL).unwrap();
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
    let out = read_new(SurfaceKind::Claude, &p, 0, fallback(), ALL).unwrap();
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
        ALL,
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
    let out = read_new(SurfaceKind::Claude, &p, 0, fallback(), ALL).unwrap();
    assert_eq!(out.next_offset, std::fs::metadata(&p).unwrap().len());
    assert_eq!(out.turns.len(), 2);
    assert!(matches!(&out.turns[0], Turn::User { text, .. } if text == "héllo 日本語 🎉"));
}

#[test]
fn byte_cap_returns_whole_lines_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, format!("{HUMAN}\n{HUMAN}\n{HUMAN}\n")).unwrap();
    let line = HUMAN.len() as u64 + 1;
    // Window holds one and a half lines: only the whole one is taken.
    let cap = line + line / 2;
    let (mut from, mut turns) = (0, 0);
    for _ in 0..3 {
        let out = read_new(SurfaceKind::Claude, &p, from, fallback(), cap).unwrap();
        assert_eq!(out.next_offset, from + line);
        assert_eq!(out.turns.len(), 1);
        turns += 1;
        from = out.next_offset;
    }
    let out = read_new(SurfaceKind::Claude, &p, from, fallback(), cap).unwrap();
    assert_eq!((out.next_offset, out.turns.len(), turns), (from, 0, 3));
}

#[test]
fn over_long_line_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    let long = format!(
        r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
        "x".repeat(5_000)
    );
    std::fs::write(&p, format!("{long}\n{HUMAN}\n")).unwrap();
    let out = read_new(SurfaceKind::Claude, &p, 0, fallback(), 1_000).unwrap();
    assert_eq!(out.next_offset, long.len() as u64 + 1);
    assert!(out.turns.is_empty() && out.lines.is_empty());
    let out = read_new(SurfaceKind::Claude, &p, out.next_offset, fallback(), 1_000).unwrap();
    assert_eq!(out.turns.len(), 1);
    // Unterminated over-long line: wait for the rest.
    std::fs::write(&p, &long).unwrap();
    let out = read_new(SurfaceKind::Claude, &p, 0, fallback(), 1_000).unwrap();
    assert_eq!((out.next_offset, out.turns.len()), (0, 0));
}

#[test]
fn line_start_at_aligns_forward() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, "ab\ncd\nef").unwrap();
    let got: Vec<_> = [0, 1, 3, 4, 7, 8, 99]
        .iter()
        .map(|&at| line_start_at(&p, at).unwrap())
        .collect();
    assert_eq!(got, [0, 3, 3, 6, 8, 8, 8]);
}

#[test]
fn lines_keep_only_timestamp_and_message() {
    let out = read_new(
        SurfaceKind::Claude,
        &fixture("claude_mixed.jsonl"),
        0,
        fallback(),
        ALL,
    )
    .unwrap();
    let keys: std::collections::BTreeSet<_> = out
        .lines
        .iter()
        .flat_map(|l| l.as_object().unwrap().keys().cloned())
        .collect();
    assert_eq!(keys, ["message", "timestamp"].map(String::from).into());
}

/// Turns from a transcript holding just `line`.
fn turns_of(line: &str) -> Vec<Turn> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.jsonl");
    std::fs::write(&p, format!("{line}\n")).unwrap();
    read_new(SurfaceKind::Claude, &p, 0, fallback(), ALL)
        .unwrap()
        .turns
}

fn injected(text: &str) -> Vec<Turn> {
    vec![Turn::Agent {
        at: fallback(),
        text: text.into(),
        kind: AgentKind::Injected,
    }]
}

#[test]
fn meta_and_compact_summary_lines_are_dropped() {
    for line in [
        r#"{"type":"user","isMeta":true,"message":{"content":"Base directory for this skill: /x"},"origin":{"kind":"human"}}"#,
        r#"{"type":"user","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"message":{"content":"This session is being continued from a previous conversation."}}"#,
    ] {
        assert_eq!(turns_of(line), vec![], "{line}");
    }
}

#[test]
fn sdk_prompts_with_human_origin_are_injected() {
    let a = r#"{"type":"user","message":{"content":"You keep the notes for one workstream."},"origin":{"kind":"human"},"promptSource":"sdk"}"#;
    let b = r#"{"type":"user","message":{"content":"You keep the notes for one workstream."},"origin":{"kind":"human"},"entrypoint":"sdk-ts"}"#;
    for line in [a, b] {
        assert_eq!(
            turns_of(line),
            injected("You keep the notes for one workstream.")
        );
    }
}

#[test]
fn pasted_blin_letter_is_injected() {
    let line = r#"{"type":"user","message":{"content":"<pasted_content id=\"f5a0\">\nProtocol: blin-letter/1\nFrom: x\n</pasted_content id=\"f5a0\">"},"origin":{"kind":"human"},"promptSource":"typed"}"#;
    let text = "<pasted_content id=\"f5a0\">\nProtocol: blin-letter/1\nFrom: x\n</pasted_content id=\"f5a0\">";
    assert_eq!(turns_of(line), injected(text));
    // Any other paste is still the user's input.
    let other = line.replace("Protocol: blin-letter/1", "my notes");
    assert!(matches!(&turns_of(&other)[..], [Turn::User { .. }]));
}

#[test]
fn slash_command_renders_as_typed() {
    let line = r#"{"type":"user","message":{"content":"<command-message>incident</command-message>\n<command-name>/incident</command-name>\n<command-args>setup now</command-args>"},"origin":{"kind":"human"}}"#;
    assert_eq!(
        turns_of(line),
        vec![Turn::User {
            at: fallback(),
            text: "/incident setup now".into()
        }]
    );
}
