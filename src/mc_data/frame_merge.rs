//! Merge overlapping terminal screen captures into a deduplicated transcript.
//!
//! Remote agent surfaces (a `mosh`/`ssh` pane to a box without cmux) have no
//! agent-event bridge, so mc can only learn their intent by *watching the
//! screen*. We capture the pane every few seconds, but consecutive captures
//! overlap heavily — capture N+1 is usually capture N scrolled up by a handful
//! of lines. Naive line-set dedup is wrong (it destroys legitimate repeats and
//! ordering); what we need is to recover the **scroll delta** and append only
//! the lines that genuinely scrolled in.
//!
//! ## How it works
//!
//! 1. **Universal strip** — remove terminal-universal noise (ANSI/CSI escapes,
//!    trailing whitespace). Format-independent, so safe to strip blind.
//! 2. **Region exclusion** — drop the bottom chrome (the input composer box and
//!    any tmux status bar). That chrome stays pinned to the bottom while the
//!    transcript scrolls, so leaving it in would make it vote `delta = 0` and
//!    fight the transcript's real scroll.
//! 3. **Anchor-vote scroll delta** — high-entropy lines (long, many distinct
//!    chars) are alignment anchors; a spinner/timer line never qualifies. Each
//!    line unique-and-anchored in *both* frames votes `d = i_prev - j_cap`; the
//!    consensus wins. Volatile lines can't corrupt the alignment because they
//!    don't anchor.
//! 4. **Learned volatile masking** — once two lines are *proven* to be the same
//!    logical line (aligned via the delta), whatever differs between them is, by
//!    definition, the volatile span. We learn it by diff — never a hardcoded
//!    `\d+s` catalog — and use it to peel the live status line off the tail.
//!
//! Validated against real `mosh`→labs Claude captures (see
//! `tests/mc_data_frame_merge.rs`): 100+ anchors agree on the delta every
//! frame, idle ticks append nothing, scrolls extract exactly the new lines.
//!
//! This module is phase 1 of the remote-surface-intent feature; the 5s grab
//! loop, change-gated LLM inference, and `~/data/mission-control` cache are
//! documented in `docs/plans/remote-surface-intent.md` and wired in later.
#![allow(dead_code)] // wired into the live grab loop in a later phase

use std::collections::{HashMap, HashSet};

/// Minimum trimmed length for a line to be an alignment anchor.
const ANCHOR_MIN_LEN: usize = 12;
/// Minimum distinct alphanumeric characters for a line to be an anchor.
const ANCHOR_MIN_DISTINCT: usize = 6;
/// Minimum agreeing anchors before we trust a scroll delta.
const MIN_AGREEING_ANCHORS: usize = 2;
/// A composer/box rule is a run of `─` at least this long.
const RULE_MIN_LEN: usize = 30;

/// Strip terminal-universal noise: CSI escape sequences + trailing whitespace.
/// Everything UI-specific (timers, spinners, token counts) is learned later by
/// diffing aligned frames, not stripped here.
pub fn strip_universal(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // ESC — consume a CSI sequence (ESC '[' … final-byte in @..~).
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out.trim_end().to_string()
}

/// Normalize a raw multi-line capture into stripped lines.
pub fn normalize(raw: &str) -> Vec<String> {
    raw.lines().map(strip_universal).collect()
}

/// Is this a composer/box horizontal rule (a run of `─`)?
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.chars().count() >= RULE_MIN_LEN && t.chars().all(|c| c == '\u{2500}' || c == ' ')
}

/// Drop the bottom UI chrome (input composer box + tmux status bar). The
/// composer is bracketed by `─` rules around a `❯` prompt; everything from the
/// rule above that prompt downward is chrome and is removed.
pub fn transcript_region(frame: &[String]) -> Vec<String> {
    let prompt = frame
        .iter()
        .rposition(|l| l.trim_start().starts_with('\u{276f}'));
    let cut = match prompt {
        Some(q) => (0..q).rev().find(|&i| is_rule(&frame[i])).unwrap_or(q),
        None => {
            // No composer prompt visible: fall back to the last rule in the
            // bottom third, else keep everything.
            let start = frame.len() * 2 / 3;
            (start..frame.len())
                .rev()
                .find(|&i| is_rule(&frame[i]))
                .unwrap_or(frame.len())
        }
    };
    frame[..cut].to_vec()
}

/// A good alignment anchor: long, high-entropy. Status/spinner lines never
/// qualify, which is exactly why volatile lines can't corrupt the alignment.
fn is_anchor(line: &str) -> bool {
    let t = line.trim();
    if t.chars().count() < ANCHOR_MIN_LEN {
        return false;
    }
    let distinct: HashSet<char> = t
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    distinct.len() >= ANCHOR_MIN_DISTINCT
}

/// Anchor lines that occur exactly once in the frame → their index. Repeated
/// anchors are dropped (they can't cast an unambiguous vote).
fn unique_anchor_index(frame: &[String]) -> HashMap<&str, usize> {
    let mut count: HashMap<&str, usize> = HashMap::new();
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for (i, line) in frame.iter().enumerate() {
        if !is_anchor(line) {
            continue;
        }
        *count.entry(line.as_str()).or_insert(0) += 1;
        idx.insert(line.as_str(), i);
    }
    idx.into_iter().filter(|(k, _)| count[k] == 1).collect()
}

/// Scroll delta `d` (how many lines `cap` advanced past `prev`) by anchor
/// voting. Returns `(delta, agreeing_votes)`. `delta == None` means no
/// confident overlap (a gap, or unrelated frames).
pub fn scroll_delta(prev: &[String], cap: &[String]) -> (Option<isize>, usize) {
    let pidx = unique_anchor_index(prev);
    let cidx = unique_anchor_index(cap);
    let mut votes: HashMap<isize, usize> = HashMap::new();
    for (line, &i) in &pidx {
        if let Some(&j) = cidx.get(line) {
            *votes.entry(i as isize - j as isize).or_insert(0) += 1;
        }
    }
    match votes.iter().max_by_key(|&(_, &n)| n) {
        Some((&d, &n)) if n >= MIN_AGREEING_ANCHORS => (Some(d), n),
        _ => (None, 0),
    }
}

/// Given `prev` and `cap` already aligned, the lines that scrolled in.
/// `None` delta → no overlap → treat the whole frame as new (a gap).
pub fn new_lines(prev: &[String], cap: &[String]) -> Vec<String> {
    match scroll_delta(prev, cap).0 {
        Some(0) => vec![],
        Some(d) if d > 0 && (d as usize) <= cap.len() => cap[cap.len() - d as usize..].to_vec(),
        _ => cap.to_vec(),
    }
}

/// Two lines we've proven are the same logical line: mask the differing span
/// (the volatile part) with `§`. Equal lines pass through unchanged. v1 masks a
/// single prefix→suffix span; a token-level diff would tighten multi-span
/// status lines but isn't needed for stable hashing.
pub fn mask_volatile(a: &str, b: &str) -> String {
    if a == b {
        return a.to_string();
    }
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    let mut p = 0;
    while p < av.len() && p < bv.len() && av[p] == bv[p] {
        p += 1;
    }
    let (mut sa, mut sb) = (av.len(), bv.len());
    while sa > p && sb > p && av[sa - 1] == bv[sb - 1] {
        sa -= 1;
        sb -= 1;
    }
    let prefix: String = av[..p].iter().collect();
    let suffix: String = av[sa..].iter().collect();
    format!("{prefix}§{suffix}")
}

/// How much of `mask_volatile`'s output is stable (non-`§`) alphanumeric
/// content. Near 1.0 = a small mutation of an otherwise identical line (a
/// status tick); near 0.0 = an entirely different line.
fn stable_ratio(mask: &str) -> f32 {
    let alnum = mask.chars().filter(|c| c.is_ascii_alphanumeric()).count();
    let total = mask.chars().filter(|c| !c.is_whitespace()).count().max(1);
    alnum as f32 / total as f32
}

/// A trailing line is the *live status line* if, versus the previous frame's
/// trailing line, it changed only in volatile spans (shared skeleton). Learned
/// by diff — no hardcoded spinner/timer pattern.
fn is_status_update(prev_tail: &str, cur_tail: &str) -> bool {
    if prev_tail == cur_tail {
        return false;
    }
    let mask = mask_volatile(prev_tail, cur_tail);
    // It mutated (mask has a §) but kept a substantial stable skeleton.
    mask.contains('§') && stable_ratio(&mask) >= 0.5
}

fn last_nonblank(lines: &[String]) -> Option<usize> {
    (0..lines.len()).rev().find(|&i| !lines[i].trim().is_empty())
}

/// Stateful accumulator: feed it raw captures, it appends only the genuinely
/// new transcript lines and tracks the current live status line separately.
#[derive(Default)]
pub struct FrameMerger {
    /// Previous frame's transcript region (status line peeled).
    prev_body: Option<Vec<String>>,
    /// Previous frame's raw trailing line (for status-update detection).
    prev_tail: Option<String>,
    /// Skeleton (`mask_volatile` output) of the live status line, so stale
    /// copies that scroll up into history can be filtered out of the transcript.
    status_skeleton: Option<String>,
    /// Accumulated, deduplicated transcript (bounded ring).
    pub transcript: Vec<String>,
    /// The current live status line, if any (peeled off the tail).
    pub status: Option<String>,
    /// Max retained transcript lines (0 = unbounded).
    max_lines: usize,
}

impl FrameMerger {
    pub fn new(max_lines: usize) -> Self {
        Self {
            max_lines,
            ..Default::default()
        }
    }

    /// Ingest one raw capture; returns the number of new transcript lines
    /// appended (0 on an idle tick).
    pub fn ingest(&mut self, raw: &str) -> usize {
        let region = transcript_region(&normalize(raw));
        let (body, status) = self.split_status(&region);

        let appended: Vec<String> = match &self.prev_body {
            None => body.clone(),
            Some(prev) => new_lines(prev, &body),
        };

        // Drop any stale copy of the live status line that scrolled up into the
        // new content (its skeleton matches the current status line).
        let skeleton = self.status_skeleton.clone();
        let kept: Vec<String> = appended
            .into_iter()
            .filter(|l| match &skeleton {
                Some(sk) => !same_skeleton(l, sk),
                None => true,
            })
            .collect();

        let n = kept.len();
        self.transcript.extend(kept);
        self.trim();

        self.prev_tail = body.last().cloned();
        self.prev_body = Some(body);
        self.status = status;
        n
    }

    /// Split the live status line off the tail using the learned-volatile test
    /// against the previous frame's tail.
    fn split_status(&mut self, region: &[String]) -> (Vec<String>, Option<String>) {
        let Some(li) = last_nonblank(region) else {
            return (region.to_vec(), None);
        };
        let tail = &region[li];
        if let Some(prev_tail) = &self.prev_tail
            && is_status_update(prev_tail, tail)
        {
            self.status_skeleton = Some(mask_volatile(prev_tail, tail));
            let body: Vec<String> = region[..li].to_vec();
            return (body, Some(tail.clone()));
        }
        (region.to_vec(), None)
    }

    fn trim(&mut self) {
        if self.max_lines > 0 && self.transcript.len() > self.max_lines {
            let excess = self.transcript.len() - self.max_lines;
            self.transcript.drain(0..excess);
        }
    }
}

/// Does `line` share the volatile-skeleton `skeleton` (`prefix§suffix`)? True
/// when `line` starts with the skeleton's stable prefix and ends with its
/// stable suffix — i.e. it's another tick of the same live status line.
fn same_skeleton(line: &str, skeleton: &str) -> bool {
    // The skeleton is `prefix§suffix`. A matching line starts with prefix and
    // ends with suffix (ignoring the volatile middle).
    if let Some((prefix, suffix)) = skeleton.split_once('§') {
        let lt = line.trim();
        (prefix.is_empty() || lt.starts_with(prefix.trim_start()))
            && (suffix.is_empty() || lt.ends_with(suffix.trim_end()))
            && lt.chars().count() >= prefix.trim().chars().count()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_marks_changed_span_learned_not_cataloged() {
        // Equal lines pass through untouched.
        assert_eq!(mask_volatile("same line here", "same line here"), "same line here");
        // A changed span is masked with §, stable parts preserved — no `\d+s` rule.
        let m = mask_volatile("Thinking (12s)", "Thinking (17s)");
        assert!(m.contains('§'), "expected a masked span, got {m}");
        assert!(m.starts_with("Thinking ("), "stable prefix kept: {m}");
        // A ticking status line is recognized as the same live line by diff alone.
        assert!(is_status_update(
            "✻ Working (1m9s · ↓ 3.4k tokens)",
            "✻ Working (1m14s · ↓ 3.7k tokens)"
        ));
        // A genuinely different line is NOT treated as a status update.
        assert!(!is_status_update(
            "✻ Working (1m9s · ↓ 3.4k tokens)",
            "● Let me look at the auth module instead"
        ));
    }

    #[test]
    fn idle_tick_yields_no_scroll() {
        // Two stable anchors + one volatile status line that ticks. The merge
        // must read this as delta 0 (nothing scrolled), not invent new lines.
        let prev = vec![
            "the bug is a missing await in auth".to_string(),
            "patched the logout handler as well".to_string(),
            "✻ Working (1m9s · 3.4k tokens)".to_string(),
        ];
        let cap = vec![
            "the bug is a missing await in auth".to_string(),
            "patched the logout handler as well".to_string(),
            "✻ Working (1m14s · 3.4k tokens)".to_string(),
        ];
        assert_eq!(scroll_delta(&prev, &cap).0, Some(0));
        assert!(new_lines(&prev, &cap).is_empty());
    }

    #[test]
    fn anchor_vote_finds_scroll_despite_volatile_overlap() {
        let prev: Vec<String> = [
            "fix the failing test in auth.rs",
            "test auth::login ... FAILED",
            "Thinking (12s tokens)",
            "the bug is a missing await now",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cap: Vec<String> = [
            "test auth::login ... FAILED",
            "Thinking (88s tokens)", // volatile line changed inside overlap
            "the bug is a missing await now",
            "test auth::logout ... ok now",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(scroll_delta(&prev, &cap).0, Some(1));
        assert_eq!(new_lines(&prev, &cap), vec!["test auth::logout ... ok now"]);
    }

    #[test]
    fn transcript_region_cuts_composer() {
        let frame: Vec<String> = [
            "real transcript content line here",
            "\u{2500}".repeat(40).as_str(),
            "\u{276f} ",
            "\u{2500}".repeat(40).as_str(),
            "  bypass permissions on",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let region = transcript_region(&frame);
        assert_eq!(region, vec!["real transcript content line here".to_string()]);
    }
}
