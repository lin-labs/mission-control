use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::Value;

fn mc_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("mission-control")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/updates")
        .join(name)
}

/// Copies `name` to `$HOME/<rel>` and returns the path.
fn place(home: &Path, rel: &str, name: &str) -> PathBuf {
    let p = home.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::copy(fixture(name), &p).unwrap();
    p
}

fn append(p: &Path, s: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
    writeln!(f, "{s}").unwrap();
}

fn watch(home: &Path) {
    let out = Command::new(mc_bin())
        .env("HOME", home)
        .args(["watch", "--once", "--seed-only"])
        .output()
        .expect("run watch");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn updates(home: &Path) -> PathBuf {
    home.join("data/mission-control/updates/local")
}

fn record(home: &Path, session: &str) -> Option<Value> {
    let raw = std::fs::read(updates(home).join(format!("{session}.json"))).ok()?;
    Some(serde_json::from_slice(&raw).unwrap())
}

fn inputs(rec: &Value) -> usize {
    rec["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["user_inputs"].as_array().unwrap().len())
        .sum()
}

const HUMAN: &str = r#"{"type":"user","message":{"content":"one more thing"},"timestamp":"2026-11-02T06:00:00Z","origin":{"kind":"human"}}"#;
const AGENT: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]},"timestamp":"2026-11-02T06:00:00Z"}"#;

#[test]
fn watch_once_seed_only_writes_record() {
    let home = tempfile::tempdir().unwrap();
    place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    watch(home.path());
    let rec = record(home.path(), "abc").expect("abc.json");
    assert_eq!(rec["segments"].as_array().unwrap().len(), 1);
    // The fixture has two human inputs (a string line and an array line).
    assert_eq!(inputs(&rec), 2);
    assert_eq!(rec["agent"], "claude");
    assert_eq!(rec["session"], "abc");
    let mon: Value =
        serde_json::from_slice(&std::fs::read(updates(home.path()).join("monitor.json")).unwrap())
            .unwrap();
    for k in ["host", "version", "pid", "last_tick"] {
        assert!(!mon[k].is_null(), "monitor.json lacks {k}: {mon}");
    }
}

#[test]
fn watch_twice_reads_only_appended() {
    let home = tempfile::tempdir().unwrap();
    let t = place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    watch(home.path());
    append(&t, HUMAN);
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 3);
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 3);
}

#[test]
fn first_run_ignores_old_transcripts() {
    let home = tempfile::tempdir().unwrap();
    let t = place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    let old = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&t)
        .unwrap()
        .set_modified(old)
        .unwrap();
    watch(home.path());
    assert!(record(home.path(), "abc").is_none());
    // It is tracked at its length, so only growth is read.
    append(&t, HUMAN);
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 1);
}

#[test]
fn subagent_transcripts_are_not_watched() {
    let home = tempfile::tempdir().unwrap();
    place(
        home.path(),
        ".claude/projects/-x/abc/subagents/agent-1.jsonl",
        "claude_mixed.jsonl",
    );
    watch(home.path());
    assert!(record(home.path(), "agent-1").is_none());
    assert!(record(home.path(), "abc").is_none());
}

#[test]
fn session_without_user_turn_gets_record_only_after_input() {
    let home = tempfile::tempdir().unwrap();
    let t = home.path().join(".claude/projects/-x/abc.jsonl");
    std::fs::create_dir_all(t.parent().unwrap()).unwrap();
    std::fs::write(&t, format!("{AGENT}\n")).unwrap();
    watch(home.path());
    assert!(record(home.path(), "abc").is_none());
    append(&t, HUMAN);
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 1);
}

#[test]
fn codex_rollouts_get_records() {
    let home = tempfile::tempdir().unwrap();
    place(
        home.path(),
        ".codex/sessions/2026/11/04/rollout-x.jsonl",
        "codex_user.jsonl",
    );
    watch(home.path());
    let rec = record(home.path(), "rollout-x").expect("codex record");
    assert_eq!(rec["agent"], "codex");
    assert_eq!(inputs(&rec), 1);
    assert!(rec["surface"].is_null());
}

#[test]
fn updates_dir_flag_redirects_output() {
    let home = tempfile::tempdir().unwrap();
    let out_dir = tempfile::tempdir().unwrap();
    place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    let out = Command::new(mc_bin())
        .env("HOME", home.path())
        .args(["watch", "--once", "--seed-only", "--updates-dir"])
        .arg(out_dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out_dir.path().join("abc.json").exists());
    assert!(out_dir.path().join("monitor.json").exists());
    assert!(!updates(home.path()).exists());
}

#[test]
fn truncated_transcript_rereads_only_newer_turns() {
    let home = tempfile::tempdir().unwrap();
    let t = place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    watch(home.path());
    // Compacted: shorter than the stored offset, so it is re-read from 0.
    let head: Vec<_> = std::fs::read_to_string(&t)
        .unwrap()
        .lines()
        .take(2)
        .map(str::to_string)
        .collect();
    std::fs::write(&t, format!("{}\n{HUMAN}\n", head.join("\n"))).unwrap();
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 3);
}
