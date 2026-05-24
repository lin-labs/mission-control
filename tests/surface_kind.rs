//! Tests for `mc_data::surface_kind`.
//!
//! We test the pure `from_comm` mapping (the hot path that actually matters
//! for correctness), serde round-trip, and the `effective_kind` "agent just
//! exited" grace-period logic. We don't try to test the lsof/ps detection
//! pipeline directly — that depends on a live tty and is flaky in CI.
//!
//! The persistence tests touch ~/data/mission-control/.data/<uuid>/surfaces/
//! via the production helpers, using deterministic per-test UUIDs prefixed
//! with `test-surface-kind-` so they don't collide with real workspace data
//! and clean up after themselves. The repo convention is to run the test
//! suite with `--test-threads=1`, which keeps these from racing each other.

use chrono::{Duration, Utc};
use mission_control::mc_data::surface_kind::{
    self, LastAgent, SurfaceKind, effective_kind, read_last_agent, write_last_agent,
};

// ── from_comm ─────────────────────────────────────────────────────────────

#[test]
fn from_comm_basic_names() {
    assert_eq!(SurfaceKind::from_comm("claude"), SurfaceKind::Claude);
    assert_eq!(SurfaceKind::from_comm("codex"), SurfaceKind::Codex);
    assert_eq!(
        SurfaceKind::from_comm("cursor-agent"),
        SurfaceKind::OtherAgent
    );
    assert_eq!(SurfaceKind::from_comm("aider"), SurfaceKind::OtherAgent);
    assert_eq!(SurfaceKind::from_comm("goose"), SurfaceKind::OtherAgent);
    assert_eq!(SurfaceKind::from_comm("zsh"), SurfaceKind::Shell);
    assert_eq!(SurfaceKind::from_comm("bash"), SurfaceKind::Shell);
    assert_eq!(SurfaceKind::from_comm("fish"), SurfaceKind::Shell);
    assert_eq!(SurfaceKind::from_comm("sh"), SurfaceKind::Shell);
}

#[test]
fn from_comm_strips_path() {
    assert_eq!(
        SurfaceKind::from_comm("/opt/homebrew/bin/claude"),
        SurfaceKind::Claude
    );
    assert_eq!(
        SurfaceKind::from_comm("/usr/local/bin/codex"),
        SurfaceKind::Codex
    );
}

#[test]
fn from_comm_strips_login_dash() {
    // macOS reports login shells as "-/bin/zsh" or "-zsh".
    assert_eq!(SurfaceKind::from_comm("-/bin/zsh"), SurfaceKind::Shell);
    assert_eq!(SurfaceKind::from_comm("-zsh"), SurfaceKind::Shell);
    assert_eq!(SurfaceKind::from_comm("-bash"), SurfaceKind::Shell);
}

#[test]
fn from_comm_trims_whitespace() {
    assert_eq!(SurfaceKind::from_comm("  claude  "), SurfaceKind::Claude);
    assert_eq!(SurfaceKind::from_comm("zsh\n"), SurfaceKind::Shell);
}

#[test]
fn from_comm_unknown_for_other_programs() {
    assert_eq!(SurfaceKind::from_comm("vim"), SurfaceKind::Unknown);
    assert_eq!(SurfaceKind::from_comm("nvim"), SurfaceKind::Unknown);
    assert_eq!(SurfaceKind::from_comm(""), SurfaceKind::Unknown);
    assert_eq!(SurfaceKind::from_comm("   "), SurfaceKind::Unknown);
}

#[test]
fn glyphs_are_stable() {
    assert_eq!(SurfaceKind::Claude.glyph(), '✻');
    assert_eq!(SurfaceKind::Codex.glyph(), '▲');
    assert_eq!(SurfaceKind::OtherAgent.glyph(), '◆');
    assert_eq!(SurfaceKind::Shell.glyph(), '$');
    assert_eq!(SurfaceKind::Unknown.glyph(), '·');
}

#[test]
fn is_agent_is_correct() {
    assert!(SurfaceKind::Claude.is_agent());
    assert!(SurfaceKind::Codex.is_agent());
    assert!(SurfaceKind::OtherAgent.is_agent());
    assert!(!SurfaceKind::Shell.is_agent());
    assert!(!SurfaceKind::Unknown.is_agent());
}

// ── serde round-trip ──────────────────────────────────────────────────────

#[test]
fn surface_kind_serde_roundtrip() {
    for k in [
        SurfaceKind::Claude,
        SurfaceKind::Codex,
        SurfaceKind::OtherAgent,
        SurfaceKind::Shell,
        SurfaceKind::Unknown,
    ] {
        let s = serde_json::to_string(&k).expect("serialize");
        let back: SurfaceKind = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(k, back, "round-trip failed for {:?}", k);
    }
}

#[test]
fn surface_kind_serde_snake_case() {
    // Confirm the wire format is snake_case so JSON files on disk remain
    // human-friendly. `other_agent` not `OtherAgent`.
    let s = serde_json::to_string(&SurfaceKind::OtherAgent).unwrap();
    assert_eq!(s, "\"other_agent\"");
    let s = serde_json::to_string(&SurfaceKind::Claude).unwrap();
    assert_eq!(s, "\"claude\"");
}

#[test]
fn last_agent_json_shape() {
    let snap = LastAgent {
        kind: SurfaceKind::Claude,
        ts: Utc::now(),
    };
    let body = serde_json::to_string(&snap).unwrap();
    assert!(body.contains("\"kind\":\"claude\""));
    assert!(body.contains("\"ts\":"));
    let parsed: LastAgent = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed.kind, SurfaceKind::Claude);
}

// ── last-agent persistence + effective_kind ───────────────────────────────

fn cleanup(uuid: &str) {
    let dir = mission_control::mc_data::paths::surfaces_dir(uuid);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_last_agent_noop_for_shell_or_unknown() {
    let uuid = "test-surface-kind-noop";
    cleanup(uuid);
    write_last_agent(uuid, "surface:1", SurfaceKind::Shell).unwrap();
    write_last_agent(uuid, "surface:2", SurfaceKind::Unknown).unwrap();
    assert!(read_last_agent(uuid, "surface:1").is_none());
    assert!(read_last_agent(uuid, "surface:2").is_none());
    cleanup(uuid);
}

#[test]
fn write_then_read_last_agent_roundtrip() {
    let uuid = "test-surface-kind-roundtrip";
    cleanup(uuid);
    write_last_agent(uuid, "surface:42", SurfaceKind::Claude).unwrap();
    let read = read_last_agent(uuid, "surface:42").expect("snapshot should exist");
    assert_eq!(read.kind, SurfaceKind::Claude);
    // ts should be within the last few seconds.
    let age = (Utc::now() - read.ts).num_seconds().abs();
    assert!(age < 5, "ts should be recent, was {}s old", age);
    cleanup(uuid);
}

#[test]
fn effective_kind_returns_current_when_agent() {
    let uuid = "test-surface-kind-effective-agent";
    cleanup(uuid);
    // No file written; current is an agent → returns current.
    assert_eq!(
        effective_kind(uuid, "surface:1", SurfaceKind::Codex),
        SurfaceKind::Codex
    );
    cleanup(uuid);
}

#[test]
fn effective_kind_surfaces_recent_agent_when_current_is_shell() {
    let uuid = "test-surface-kind-effective-recent";
    cleanup(uuid);
    write_last_agent(uuid, "surface:7", SurfaceKind::Claude).unwrap();
    // Current is Shell but last-agent file is fresh → Claude wins.
    assert_eq!(
        effective_kind(uuid, "surface:7", SurfaceKind::Shell),
        SurfaceKind::Claude
    );
    // Same for Unknown.
    assert_eq!(
        effective_kind(uuid, "surface:7", SurfaceKind::Unknown),
        SurfaceKind::Claude
    );
    cleanup(uuid);
}

#[test]
fn effective_kind_ignores_stale_snapshot() {
    let uuid = "test-surface-kind-effective-stale";
    cleanup(uuid);
    // Write a snapshot whose ts is older than the 5-minute grace window.
    let dir = mission_control::mc_data::paths::surfaces_dir(uuid);
    std::fs::create_dir_all(&dir).unwrap();
    let snap = LastAgent {
        kind: SurfaceKind::Claude,
        ts: Utc::now() - Duration::seconds(600),
    };
    // Use the same filename scheme the production helper uses.
    let fname = format!("{}.last-agent", "surface:7".replace([':', '/', '\\'], "_"));
    std::fs::write(dir.join(&fname), serde_json::to_string(&snap).unwrap()).unwrap();

    // Stale snapshot → effective_kind should fall back to `current`.
    assert_eq!(
        effective_kind(uuid, "surface:7", SurfaceKind::Shell),
        SurfaceKind::Shell
    );
    cleanup(uuid);
}

#[test]
fn detect_returns_unknown_for_bogus_tty() {
    // Detection on a tty path that doesn't exist must not panic and must
    // return Unknown. This is the contract refresh_workspaces relies on.
    assert_eq!(
        surface_kind::detect("ttys_does_not_exist_9999"),
        SurfaceKind::Unknown
    );
}
