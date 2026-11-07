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
    // fs::copy keeps the fixture's mtime on macOS; a fresh file is "recent".
    std::fs::write(&p, std::fs::read(fixture(name)).unwrap()).unwrap();
    p
}

fn append(p: &Path, s: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
    writeln!(f, "{s}").unwrap();
}

fn watch(home: &Path) {
    watch_with(home, &[]);
}

fn watch_with(home: &Path, extra: &[&str]) {
    let out = Command::new(mc_bin())
        .env("HOME", home)
        .args(["watch", "--once", "--seed-only"])
        .args(extra)
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
    set_old(&t);
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

fn set_old(p: &Path) {
    let old = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(old)
        .unwrap();
}

#[test]
fn lost_offset_entry_does_not_duplicate_turns() {
    let home = tempfile::tempdir().unwrap();
    place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    watch(home.path());
    let offsets = updates(home.path()).join(".offsets.json");
    let mut m: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&offsets).unwrap()).unwrap();
    m.retain(|k, _| !k.ends_with("abc.jsonl"));
    std::fs::write(&offsets, serde_json::to_vec(&m).unwrap()).unwrap();
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 2);
}

#[test]
fn unknown_old_transcript_on_later_run_reads_only_growth() {
    let home = tempfile::tempdir().unwrap();
    watch(home.path()); // first run: nothing to seed
    let t = place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    set_old(&t);
    watch(home.path());
    assert!(record(home.path(), "abc").is_none());
    append(&t, HUMAN);
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 1);
}

#[test]
fn failed_save_keeps_offset() {
    let home = tempfile::tempdir().unwrap();
    let t = place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    watch(home.path());
    let rec = updates(home.path()).join("abc.json");
    let saved = std::fs::read(&rec).unwrap();
    append(&t, HUMAN);
    // A directory in the record's place makes the save's rename fail.
    std::fs::remove_file(&rec).unwrap();
    std::fs::create_dir(&rec).unwrap();
    watch(home.path());
    std::fs::remove_dir(&rec).unwrap();
    std::fs::write(&rec, saved).unwrap();
    watch(home.path());
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 3);
}

/// Fixture (2 inputs), 50 agent lines, then one more human input.
fn long_transcript(home: &Path) -> PathBuf {
    let t = place(home, ".claude/projects/-x/abc.jsonl", "claude_mixed.jsonl");
    for _ in 0..50 {
        append(&t, AGENT);
    }
    append(&t, HUMAN);
    t
}

#[test]
fn seed_starts_near_the_tail() {
    let home = tempfile::tempdir().unwrap();
    long_transcript(home.path());
    watch_with(home.path(), &["--seed-bytes", "2000"]);
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 1);
}

#[test]
fn small_chunks_apply_the_whole_transcript() {
    let home = tempfile::tempdir().unwrap();
    let t = long_transcript(home.path());
    watch_with(home.path(), &["--chunk-bytes", "1000"]);
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 3);
    let offsets: Value =
        serde_json::from_slice(&std::fs::read(updates(home.path()).join(".offsets.json")).unwrap())
            .unwrap();
    let len = std::fs::metadata(&t).unwrap().len();
    assert_eq!(offsets[t.to_str().unwrap()]["offset"], len);
}

fn offsets(home: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(updates(home).join(".offsets.json")).unwrap()).unwrap()
}

#[test]
fn tick_budget_resumes_next_tick() {
    let home = tempfile::tempdir().unwrap();
    let t = long_transcript(home.path());
    let len = std::fs::metadata(&t).unwrap().len();
    let args = ["--chunk-bytes", "1000", "--tick-budget-bytes", "2000"];
    let offset = || {
        offsets(home.path())[t.to_str().unwrap()]["offset"]
            .as_u64()
            .unwrap()
    };
    watch_with(home.path(), &args);
    let first = offset();
    assert!(first > 0 && first <= 2000, "{first}");
    watch_with(home.path(), &args);
    let second = offset();
    assert!(second > first && second <= first + 2000, "{first} {second}");
    for _ in 0..len / 1000 {
        watch_with(home.path(), &args);
    }
    assert_eq!(offset(), len);
    assert_eq!(inputs(&record(home.path(), "abc").unwrap()), 3);
}

#[test]
fn idle_tick_keeps_offsets_file_and_prunes_missing() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    place(
        home.path(),
        ".claude/projects/-x/abc.jsonl",
        "claude_mixed.jsonl",
    );
    watch(home.path());
    let path = updates(home.path()).join(".offsets.json");
    for p in [&path, &updates(home.path()).join("abc.json")] {
        let mode = std::fs::metadata(p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{}", p.display());
    }
    // Same content, different bytes: an idle tick must not rewrite it.
    let pretty = serde_json::to_string_pretty(&offsets(home.path())).unwrap();
    std::fs::write(&path, &pretty).unwrap();
    watch(home.path());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), pretty);
    // An entry for a missing transcript is pruned.
    let mut m = offsets(home.path());
    m["/gone.jsonl"] = m.as_object().unwrap().values().next().unwrap().clone();
    std::fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
    watch(home.path());
    assert!(offsets(home.path()).get("/gone.jsonl").is_none());
}
