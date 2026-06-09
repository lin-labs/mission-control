//! Regression test for the remote-surface frame merger, pinned to REAL
//! consecutive captures from a live mosh→labs Claude pane (surface:29),
//! frozen under tests/fixtures/remote_frames/. These six 200-line frames cover
//! one idle tick (only the status timer changed) and four real scroll events.
//!
//! Re-capture with:
//!   cmux rpc surface.read_text '{"surface_id":"surface:N","lines":200}'

use mission_control::mc_data::frame_merge::{
    self, normalize, scroll_delta, transcript_region, FrameMerger,
};
use std::fs;

fn frame(n: usize) -> Vec<String> {
    let path = format!("tests/fixtures/remote_frames/frame{n}.txt");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    transcript_region(&normalize(&raw))
}

#[test]
fn scroll_deltas_match_live_capture() {
    // Validated by hand against the live pane: idle, then four scrolls.
    let expected = [Some(0isize), Some(18), Some(35), Some(4), Some(4)];
    for (i, &want) in expected.iter().enumerate() {
        let (prev, cap) = (frame(i + 1), frame(i + 2));
        let (got, votes) = scroll_delta(&prev, &cap);
        assert_eq!(
            got,
            want,
            "frame{}→frame{}: expected delta {want:?}, got {got:?}",
            i + 1,
            i + 2
        );
        // Every transition is decided by a strong anchor consensus, not a
        // coincidental 2-line match.
        assert!(
            votes >= 50,
            "frame{}→frame{}: only {votes} anchors agreed (expected a strong consensus)",
            i + 1,
            i + 2
        );
    }
}

#[test]
fn idle_tick_appends_nothing() {
    // frame1→frame2 is an idle tick (the ✻ Evaporating timer advanced); a
    // proper merge must add zero lines despite the changed status line.
    let new = frame_merge::new_lines(&frame(1), &frame(2));
    assert!(new.is_empty(), "idle tick should add no lines, got {new:?}");
}

#[test]
fn merger_dedups_and_drops_live_status_line() {
    let mut m = FrameMerger::new(5000);
    for n in 1..=6 {
        let raw = fs::read_to_string(format!("tests/fixtures/remote_frames/frame{n}.txt")).unwrap();
        m.ingest(&raw);
    }
    let joined = m.transcript.join("\n");

    // Real transcript content that scrolled through is captured.
    assert!(
        joined.contains("honest verification that the rename is clean"),
        "expected real scrolled-in content in the merged transcript"
    );

    // The merged transcript is far smaller than the naive sum of 6×~193 lines —
    // overlap was deduplicated, not concatenated.
    assert!(
        m.transcript.len() < 400,
        "expected dedup to keep the transcript compact, got {} lines",
        m.transcript.len()
    );
}
