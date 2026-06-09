//! Read cmux's authoritative per-surface agent binding — the "cmux bind"
//! registry that its hook bridge maintains at
//! `~/.cmuxterm/{claude,codex}-hook-sessions.json`.
//!
//! Each record ties a cmux **surface UUID** to the agent session actually
//! running there: its `cwd`, native `transcriptPath`, and `agentLifecycle`.
//! This is exact (keyed by surface id), unlike mc's older fuzzy host/cwd
//! session-log matching, so it's the right source for per-surface repo and
//! intent attribution. A workspace no longer inherits a stray surface's repo,
//! and two surfaces can't be handed the same prompt.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::mc_data::surface_kind::SurfaceKind;

/// One surface's bound agent session, distilled from the cmux hook registry.
/// `cwd` drives repo attribution now; `agent`/`transcript_path`/`lifecycle` are
/// consumed by the next increment (per-surface intent from the bound transcript).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HookSession {
    pub agent: SurfaceKind,
    pub cwd: Option<PathBuf>,
    pub transcript_path: Option<PathBuf>,
    /// cmux `agentLifecycle` (e.g. "working", "needsInput", "idle").
    pub lifecycle: Option<String>,
    /// cmux `updatedAt` (unix seconds); newest record per surface wins.
    pub updated_at: f64,
}

#[derive(Deserialize)]
struct Registry {
    #[serde(default)]
    sessions: HashMap<String, RawSession>,
}

#[derive(Deserialize)]
struct RawSession {
    #[serde(default, rename = "surfaceId")]
    surface_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "agentLifecycle")]
    agent_lifecycle: Option<String>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<f64>,
}

fn registry_path(file: &str) -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".cmuxterm").join(file))
}

/// Load the cmux hook registries (Claude + Codex) keyed by **surface UUID**.
/// When a surface has several historical sessions, the most-recently-updated
/// one wins. Missing/unreadable files are ignored (returns what it can).
pub fn load_by_surface() -> HashMap<String, HookSession> {
    let mut out: HashMap<String, HookSession> = HashMap::new();
    for (file, agent) in [
        ("claude-hook-sessions.json", SurfaceKind::Claude),
        ("codex-hook-sessions.json", SurfaceKind::Codex),
    ] {
        let Some(path) = registry_path(file) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(reg) = serde_json::from_str::<Registry>(&raw) else {
            continue;
        };
        for session in reg.sessions.into_values() {
            let Some(surface_id) = session.surface_id else {
                continue;
            };
            let updated_at = session.updated_at.unwrap_or(0.0);
            let candidate = HookSession {
                agent,
                cwd: session.cwd.filter(|c| !c.is_empty()).map(PathBuf::from),
                transcript_path: session
                    .transcript_path
                    .filter(|p| !p.is_empty())
                    .map(PathBuf::from),
                lifecycle: session.agent_lifecycle.filter(|l| !l.is_empty()),
                updated_at,
            };
            match out.get(&surface_id) {
                Some(existing) if existing.updated_at >= updated_at => {}
                _ => {
                    out.insert(surface_id, candidate);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_keeps_newest_per_surface() {
        let raw = r#"{
            "version": 1,
            "sessions": {
                "s-old": {"surfaceId":"SURF-1","cwd":"/a","transcriptPath":"/t/old.jsonl","agentLifecycle":"idle","updatedAt":100.0},
                "s-new": {"surfaceId":"SURF-1","cwd":"/b","transcriptPath":"/t/new.jsonl","agentLifecycle":"working","updatedAt":200.0},
                "s-2":   {"surfaceId":"SURF-2","cwd":"","transcriptPath":"/t/2.jsonl","updatedAt":50.0}
            }
        }"#;
        let reg: Registry = serde_json::from_str(raw).unwrap();
        // Mimic load_by_surface's newest-wins merge for one file.
        let mut out: HashMap<String, HookSession> = HashMap::new();
        for session in reg.sessions.into_values() {
            let sid = session.surface_id.unwrap();
            let updated_at = session.updated_at.unwrap_or(0.0);
            let cand = HookSession {
                agent: SurfaceKind::Claude,
                cwd: session.cwd.filter(|c| !c.is_empty()).map(PathBuf::from),
                transcript_path: session.transcript_path.map(PathBuf::from),
                lifecycle: session.agent_lifecycle,
                updated_at,
            };
            match out.get(&sid) {
                Some(e) if e.updated_at >= updated_at => {}
                _ => {
                    out.insert(sid, cand);
                }
            }
        }
        // SURF-1 keeps the newer record.
        assert_eq!(out["SURF-1"].cwd, Some(PathBuf::from("/b")));
        assert_eq!(out["SURF-1"].lifecycle.as_deref(), Some("working"));
        // Empty cwd is normalized to None.
        assert_eq!(out["SURF-2"].cwd, None);
    }
}
