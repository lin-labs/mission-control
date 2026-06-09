//! Phase 2 of remote-surface intent: own the per-surface screen-grab state.
//!
//! For each remote (mosh/ssh) surface we keep a [`FrameMerger`] that stitches
//! overlapping 5s captures into a deduplicated transcript (see [`frame_merge`]).
//! This module owns the collection of mergers, an idle-backoff schedule, and a
//! debug-transcript dump under `~/data/mission-control` so the pipeline is
//! observable before phase 3 wires in LLM inference + rendering.
//!
//! It performs no capture itself — the caller (the TUI grab loop, or the
//! `remote-grab-probe` CLI) feeds raw captures via [`RemoteWatch::apply`] and
//! asks [`RemoteWatch::stride`] how often to poll a given surface.

use std::collections::HashMap;

use crate::mc_data::frame_merge::FrameMerger;
use crate::mc_data::paths;

/// Max transcript lines retained per remote surface.
const MAX_TRANSCRIPT_LINES: usize = 4000;
/// After this many consecutive idle grabs, stretch the poll interval.
const IDLE_BACKOFF_AFTER: u32 = 2;
/// Largest poll stride (in base-5s ticks) when a surface is idle (~30s).
const MAX_STRIDE: u64 = 6;

struct SurfaceState {
    merger: FrameMerger,
    /// Consecutive grabs that produced no new transcript lines.
    idle_rounds: u32,
}

impl SurfaceState {
    fn new() -> Self {
        Self {
            merger: FrameMerger::new(MAX_TRANSCRIPT_LINES),
            idle_rounds: 0,
        }
    }
}

#[derive(Default)]
pub struct RemoteWatch {
    states: HashMap<String, SurfaceState>,
    /// When false, [`apply`] skips the debug-transcript dump (used by tests so
    /// they don't write into the real `~/data/mission-control`).
    dump_enabled: bool,
}

/// What [`RemoteWatch::apply`] learned from one capture.
#[derive(Debug, Clone)]
pub struct GrabOutcome {
    pub new_lines: usize,
    pub transcript_len: usize,
    pub status: Option<String>,
}

impl RemoteWatch {
    pub fn new() -> Self {
        Self {
            dump_enabled: true,
            ..Default::default()
        }
    }

    /// Variant that never writes the debug transcript (tests).
    pub fn without_dump() -> Self {
        Self::default()
    }

    /// Poll stride (in base ticks) for a surface: 1 while it's actively
    /// changing, growing up to `MAX_STRIDE` once it's been idle a while. A
    /// surface we've never grabbed returns 1 (grab immediately).
    pub fn stride(&self, surface_ref: &str) -> u64 {
        match self.states.get(surface_ref) {
            None => 1,
            Some(s) if s.idle_rounds < IDLE_BACKOFF_AFTER => 1,
            Some(s) => (s.idle_rounds as u64 - IDLE_BACKOFF_AFTER as u64 + 2).min(MAX_STRIDE),
        }
    }

    /// Should this surface be grabbed on `tick` given its backoff stride?
    pub fn due(&self, surface_ref: &str, tick: u64) -> bool {
        let stride = self.stride(surface_ref);
        stride <= 1 || tick.is_multiple_of(stride)
    }

    /// Feed one raw capture for `surface_ref`. Updates the merger + idle
    /// counter and dumps the merged transcript for inspection.
    pub fn apply(&mut self, workspace_uuid: &str, surface_ref: &str, raw: &str) -> GrabOutcome {
        let state = self
            .states
            .entry(surface_ref.to_string())
            .or_insert_with(SurfaceState::new);
        let new_lines = state.merger.ingest(raw);
        if new_lines == 0 {
            state.idle_rounds = state.idle_rounds.saturating_add(1);
        } else {
            state.idle_rounds = 0;
        }
        let outcome = GrabOutcome {
            new_lines,
            transcript_len: state.merger.transcript.len(),
            status: state.merger.status.clone(),
        };
        // Best-effort debug dump; never let a write error disturb the loop.
        if self.dump_enabled {
            let _ = dump_transcript(workspace_uuid, surface_ref, &state.merger);
        }
        outcome
    }

    /// Drop state for surfaces no longer present (e.g. a remote pane closed).
    pub fn retain(&mut self, live_refs: &[String]) {
        self.states.retain(|k, _| live_refs.iter().any(|r| r == k));
    }

    /// Current deduplicated transcript for a surface, if tracked.
    pub fn transcript(&self, surface_ref: &str) -> Option<&[String]> {
        self.states.get(surface_ref).map(|s| s.merger.transcript.as_slice())
    }
}

fn dump_path(workspace_uuid: &str, surface_ref: &str) -> std::path::PathBuf {
    let stem = surface_ref.replace(['/', '\\', ':'], "_");
    paths::surfaces_dir(workspace_uuid).join(format!("{stem}.remote-transcript"))
}

/// Write the merged transcript (status line first) for inspection. Atomic-ish:
/// tmp file + rename. Phase 3 replaces this with a structured intent JSON.
fn dump_transcript(
    workspace_uuid: &str,
    surface_ref: &str,
    merger: &FrameMerger,
) -> std::io::Result<()> {
    let path = dump_path(workspace_uuid, surface_ref);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    if let Some(status) = &merger.status {
        body.push_str("# status: ");
        body.push_str(status);
        body.push('\n');
    }
    body.push_str(&merger.transcript.join("\n"));
    let tmp = path.with_extension("remote-transcript.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn backoff_grows_when_idle_then_resets_on_change() {
        let mut w = RemoteWatch::without_dump();
        // Unseen surface is always due.
        assert!(w.due("surface:1", 7));
        assert_eq!(w.stride("surface:1"), 1);

        // Two identical-ish frames (an idle tick) → idle_rounds climbs.
        let f = frame(&[
            "working on the rename verification step now",
            "patched the elon-id scratchpad handling too",
            "✻ Working (1m1s · 2k tokens)",
        ]);
        w.apply("ws", "surface:1", &f);
        let f2 = frame(&[
            "working on the rename verification step now",
            "patched the elon-id scratchpad handling too",
            "✻ Working (1m6s · 2k tokens)", // only the timer ticked
        ]);
        let out = w.apply("ws", "surface:1", &f2);
        assert_eq!(out.new_lines, 0, "idle tick adds no lines");
        // After enough idle rounds, stride stretches beyond 1.
        for _ in 0..4 {
            w.apply("ws", "surface:1", &f2);
        }
        assert!(w.stride("surface:1") > 1, "idle surface should back off");
    }

    #[test]
    fn retain_drops_closed_surfaces() {
        let mut w = RemoteWatch::without_dump();
        w.apply("ws", "surface:1", &frame(&["a line of real content here"]));
        w.apply("ws", "surface:2", &frame(&["another line of real content"]));
        w.retain(&["surface:1".to_string()]);
        assert!(w.transcript("surface:1").is_some());
        assert!(w.transcript("surface:2").is_none());
    }
}
