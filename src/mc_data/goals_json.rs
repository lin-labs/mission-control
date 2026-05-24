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
/// - lowercase
/// - collapse all whitespace runs to single spaces
/// - trim ascii punctuation from the trailing end
pub fn normalize_text(s: &str) -> String {
    let lower = s.to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_string()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalEntry {
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
    #[serde(default)]
    pub goals: Vec<GoalEntry>,
}

impl Default for GoalsFile {
    fn default() -> Self {
        Self {
            version: 1,
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
        self.goals.push(GoalEntry {
            text: text.to_string(),
            text_norm: norm,
            assigned_surface_ref: surface_ref.to_string(),
            assigned_agent_kind: kind,
            dispatched_at: ts,
            completed_at: None,
        });
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
