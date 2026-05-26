//! Per-workspace `goals.json` sidecar.
//!
//! trajectory.md remains the source of truth for goal text + done-state.
//! goals.json carries only assignment metadata (which surface a goal was
//! dispatched to, when, and whether the assignment has been marked done).
//!
//! The public API is intentionally unconsumed at T2-time; later tasks wire it
//! into dispatch + render paths.
#![allow(dead_code)]

use chrono::{DateTime, Utc};

use crate::mc_data::paths;
pub use crate::mc_data::surface_kind::SurfaceKind;

/// Normalize a goal text into a canonical form used for matching.
///
/// - strip a leading `[<ID>] ` badge (e.g. `[MSC-3] `) so two goals whose
///   only difference is the ID prefix still hash equal
/// - lowercase
/// - collapse all whitespace runs to single spaces
/// - trim ascii punctuation from the trailing end
pub fn normalize_text(s: &str) -> String {
    let without_id = strip_id_prefix(s).0;
    let lower = without_id.to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_string()
}

/// Detect a leading `[<PREFIX>-<N>] ` badge on a goal text. Returns
/// `(rest_without_badge, Some(extracted_id))` when present, `(input, None)`
/// otherwise. The format we recognize is:
///
/// - leading `[`
/// - 1+ uppercase ASCII letters or digits
/// - `-`
/// - 1+ digits
/// - `]`
/// - exactly one space
///
/// Anything else (including markdown links like `[link](url)`) returns
/// `(input, None)` unchanged so we never strip user text by accident.
pub fn strip_id_prefix(s: &str) -> (&str, Option<&str>) {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'[') {
        return (s, None);
    }
    // Find the closing `]`.
    let close = match s.find(']') {
        Some(i) => i,
        None => return (s, None),
    };
    // The badge has to be followed by exactly one space (`] `).
    let after = &s[close + 1..];
    let rest = match after.strip_prefix(' ') {
        Some(r) => r,
        None => return (s, None),
    };
    // Validate the inner text: <UPPER+>-<DIGIT+>.
    let inner = &s[1..close];
    let mut parts = inner.splitn(2, '-');
    let prefix = parts.next().unwrap_or("");
    let n = parts.next().unwrap_or("");
    if prefix.is_empty()
        || n.is_empty()
        || !prefix.chars().all(|c| c.is_ascii_uppercase())
        || !n.chars().all(|c| c.is_ascii_digit())
    {
        return (s, None);
    }
    (rest, Some(inner))
}

/// `true` if `s` already begins with a recognized `[<ID>] ` badge.
pub fn has_id_prefix(s: &str) -> bool {
    strip_id_prefix(s).1.is_some()
}

/// Return the input with a freshly-allocated `[<id>] ` badge in front. If a
/// badge is already present, the text is returned unchanged.
pub fn prepend_id(s: &str, id: &str) -> String {
    if has_id_prefix(s) {
        return s.to_string();
    }
    format!("[{}] {}", id, s)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalEntry {
    /// Stable short identifier of the form `<PREFIX>-<N>` (e.g. "MSC-3").
    /// `None` on pre-feature data; backfilled on next save once the
    /// workspace's prefix is known.
    #[serde(default)]
    pub id: Option<String>,
    pub text: String,
    pub text_norm: String,
    pub assigned_surface_ref: String,
    pub assigned_agent_kind: SurfaceKind,
    pub dispatched_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalsFile {
    pub version: u32,
    /// 3-letter uppercase prefix for this workspace's goal IDs (e.g. "MSC").
    /// `None` until the LLM call (or deterministic fallback) populates it.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Monotonic counter. The next allocated ID is `<prefix>-<next_seq>`.
    /// Never decrements; deleted goals do NOT free their IDs.
    #[serde(default)]
    pub next_seq: u32,
    #[serde(default)]
    pub goals: Vec<GoalEntry>,
}

impl Default for GoalsFile {
    fn default() -> Self {
        Self {
            version: 1,
            prefix: None,
            next_seq: 1,
            goals: Vec::new(),
        }
    }
}

pub fn goals_path(workspace_uuid: &str) -> std::path::PathBuf {
    paths::workspace_dir(workspace_uuid).join("goals.json")
}

impl GoalsFile {
    /// Load the goals.json for a workspace.
    ///
    /// On any failure (missing file, IO error, parse error) returns
    /// `Default::default()`. Parse errors are logged to stderr so a
    /// corrupted file is visible but never crashes the caller.
    pub fn load(workspace_uuid: &str) -> Self {
        let path = goals_path(workspace_uuid);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("goals_json: read {path:?}: {e}");
                }
                return Self::default();
            }
        };
        match serde_json::from_str::<GoalsFile>(&raw) {
            Ok(mut f) => {
                if f.version == 0 {
                    f.version = 1;
                }
                f
            }
            Err(e) => {
                eprintln!("goals_json: parse {path:?}: {e}");
                Self::default()
            }
        }
    }

    /// Atomically write goals.json. Always stamps `version = 1` before
    /// serializing.
    pub fn save(&self, workspace_uuid: &str) -> anyhow::Result<()> {
        let mut to_write = self.clone();
        to_write.version = 1;
        let path = goals_path(workspace_uuid);
        let json = serde_json::to_string_pretty(&to_write)?;
        atomic_write(&path, &json)?;
        Ok(())
    }

    /// Upsert an assignment by `text_norm`.
    ///
    /// If an existing entry matches the normalized text, its surface ref,
    /// agent kind, and dispatched timestamp are updated and any prior
    /// completed-at is cleared. Otherwise a new entry is appended.
    pub fn set_assignment(
        &mut self,
        text: &str,
        surface_ref: &str,
        kind: SurfaceKind,
        ts: DateTime<Utc>,
    ) {
        let norm = normalize_text(text);
        if let Some(existing) = self.goals.iter_mut().find(|g| g.text_norm == norm) {
            existing.assigned_surface_ref = surface_ref.to_string();
            existing.assigned_agent_kind = kind;
            existing.dispatched_at = ts;
            existing.completed_at = None;
            // Keep the original text spelling; norm is the matching key.
            return;
        }
        let id = self.allocate_id();
        self.goals.push(GoalEntry {
            id,
            text: text.to_string(),
            text_norm: norm,
            assigned_surface_ref: surface_ref.to_string(),
            assigned_agent_kind: kind,
            dispatched_at: ts,
            completed_at: None,
        });
    }

    /// Allocate the next per-workspace goal ID. Returns `None` if `prefix`
    /// hasn't been populated yet (the LLM call is still in flight); the entry
    /// gets backfilled on a later save once the prefix lands. `next_seq` is
    /// always advanced when a prefix is present, so deleted-then-re-added
    /// goals get a fresh ID.
    pub fn allocate_id(&mut self) -> Option<String> {
        let prefix = self.prefix.as_ref()?;
        if self.next_seq == 0 {
            self.next_seq = 1;
        }
        let id = format!("{}-{}", prefix, self.next_seq);
        self.next_seq += 1;
        Some(id)
    }

    /// One-shot pass: assign IDs to any goal that lacks one, in list order.
    /// No-op when `prefix.is_none()`. Returns the number of entries that
    /// gained an ID (callers can use this to decide whether to save).
    pub fn backfill_ids(&mut self) -> usize {
        if self.prefix.is_none() {
            return 0;
        }
        if self.next_seq == 0 {
            self.next_seq = 1;
        }
        let mut count = 0;
        let prefix = self.prefix.clone().unwrap();
        for g in self.goals.iter_mut() {
            if g.id.is_none() {
                g.id = Some(format!("{}-{}", prefix, self.next_seq));
                self.next_seq += 1;
                count += 1;
            }
        }
        count
    }

    /// Mark a matching entry completed. No-op if no open entry matches.
    pub fn complete(&mut self, text: &str, ts: DateTime<Utc>) {
        let norm = normalize_text(text);
        if let Some(entry) = self
            .goals
            .iter_mut()
            .find(|g| g.text_norm == norm && g.completed_at.is_none())
        {
            entry.completed_at = Some(ts);
        }
    }

    /// All open (not yet completed) entries assigned to a given surface ref.
    pub fn open_for_surface(&self, surface_ref: &str) -> Vec<&GoalEntry> {
        self.goals
            .iter()
            .filter(|g| g.assigned_surface_ref == surface_ref && g.completed_at.is_none())
            .collect()
    }

    /// First open entry matching the given text (after normalization).
    pub fn open_for_goal(&self, text: &str) -> Option<&GoalEntry> {
        let norm = normalize_text(text);
        self.goals
            .iter()
            .find(|g| g.text_norm == norm && g.completed_at.is_none())
    }
}

fn atomic_write(path: &std::path::Path, content: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for {path:?}"))?;
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("no file name for {path:?}"))?
        .to_string_lossy()
        .to_string();
    let tmp = parent.join(format!(".{}.tmp.{}", file_name, std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_id_prefix_recognizes_badge() {
        let (rest, id) = strip_id_prefix("[MSC-3] do the thing");
        assert_eq!(rest, "do the thing");
        assert_eq!(id, Some("MSC-3"));
    }

    #[test]
    fn strip_id_prefix_ignores_markdown_links() {
        let (rest, id) = strip_id_prefix("[link](http://x) other");
        assert_eq!(rest, "[link](http://x) other");
        assert!(id.is_none());
    }

    #[test]
    fn strip_id_prefix_requires_uppercase_prefix() {
        let (_, id) = strip_id_prefix("[msc-3] x");
        assert!(id.is_none());
        let (_, id2) = strip_id_prefix("[Msc-3] x");
        assert!(id2.is_none());
    }

    #[test]
    fn strip_id_prefix_requires_trailing_space() {
        // No space after `]` → not a badge.
        let (rest, id) = strip_id_prefix("[MSC-3]nospace");
        assert_eq!(rest, "[MSC-3]nospace");
        assert!(id.is_none());
    }

    #[test]
    fn prepend_id_skips_when_already_prefixed() {
        let out = prepend_id("[MSC-3] do the thing", "MSC-9");
        assert_eq!(out, "[MSC-3] do the thing");
    }

    #[test]
    fn prepend_id_adds_when_missing() {
        let out = prepend_id("do the thing", "MSC-3");
        assert_eq!(out, "[MSC-3] do the thing");
    }

    #[test]
    fn normalize_text_ignores_id_badge() {
        // Same logical goal with and without a badge must hash equal.
        let a = normalize_text("[MSC-3] do the thing.");
        let b = normalize_text("Do the thing");
        assert_eq!(a, b);
    }

    #[test]
    fn allocate_id_returns_none_without_prefix() {
        let mut g = GoalsFile::default();
        assert_eq!(g.allocate_id(), None);
        assert_eq!(g.next_seq, 1); // unchanged when no prefix
    }

    #[test]
    fn allocate_id_assigns_sequential_with_prefix() {
        let mut g = GoalsFile::default();
        g.prefix = Some("MSC".to_string());
        assert_eq!(g.allocate_id(), Some("MSC-1".to_string()));
        assert_eq!(g.allocate_id(), Some("MSC-2".to_string()));
        assert_eq!(g.allocate_id(), Some("MSC-3".to_string()));
        assert_eq!(g.next_seq, 4);
    }

    #[test]
    fn backfill_ids_assigns_in_list_order() {
        let mut g = GoalsFile::default();
        g.prefix = Some("MSC".to_string());
        g.goals = vec![
            GoalEntry {
                id: None,
                text: "first".to_string(),
                text_norm: "first".to_string(),
                assigned_surface_ref: "surface:1".to_string(),
                assigned_agent_kind: SurfaceKind::Claude,
                dispatched_at: chrono::Utc::now(),
                completed_at: None,
            },
            GoalEntry {
                id: Some("MSC-9".to_string()),
                text: "second".to_string(),
                text_norm: "second".to_string(),
                assigned_surface_ref: "surface:2".to_string(),
                assigned_agent_kind: SurfaceKind::Codex,
                dispatched_at: chrono::Utc::now(),
                completed_at: None,
            },
            GoalEntry {
                id: None,
                text: "third".to_string(),
                text_norm: "third".to_string(),
                assigned_surface_ref: "surface:3".to_string(),
                assigned_agent_kind: SurfaceKind::Claude,
                dispatched_at: chrono::Utc::now(),
                completed_at: None,
            },
        ];
        let count = g.backfill_ids();
        assert_eq!(count, 2, "only the two missing IDs should be filled");
        assert_eq!(g.goals[0].id, Some("MSC-1".to_string()));
        assert_eq!(g.goals[1].id, Some("MSC-9".to_string())); // preserved
        assert_eq!(g.goals[2].id, Some("MSC-2".to_string()));
    }

    #[test]
    fn backfill_ids_noop_without_prefix() {
        let mut g = GoalsFile::default();
        g.goals = vec![GoalEntry {
            id: None,
            text: "x".to_string(),
            text_norm: "x".to_string(),
            assigned_surface_ref: String::new(),
            assigned_agent_kind: SurfaceKind::Shell,
            dispatched_at: chrono::Utc::now(),
            completed_at: None,
        }];
        assert_eq!(g.backfill_ids(), 0);
        assert!(g.goals[0].id.is_none());
    }
}
