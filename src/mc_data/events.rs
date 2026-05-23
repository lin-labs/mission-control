use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

const MAX_EVENT_LINE_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    User,
    Agent,
    #[serde(rename = "user-undo")]
    UserUndo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Add,
    Delete,
    Edit,
    Check,
    Uncheck,
    Move,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: String, // ISO 8601 UTC
    pub source: Source,
    pub kind: Kind,
    pub section: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_explanation: Option<String>,
}

impl Event {
    pub fn new_now(source: Source, kind: Kind, section: impl Into<String>) -> Self {
        Self {
            ts: Utc::now().to_rfc3339(),
            source,
            kind,
            section: section.into(),
            before: None,
            after: None,
            snapshot: None,
            user_explanation: None,
        }
    }

    pub fn with_before(mut self, s: impl Into<String>) -> Self {
        self.before = Some(s.into());
        self
    }

    pub fn with_after(mut self, s: impl Into<String>) -> Self {
        self.after = Some(s.into());
        self
    }

    pub fn with_snapshot(mut self, n: u32) -> Self {
        self.snapshot = Some(n);
        self
    }

    pub fn with_explanation(mut self, s: impl Into<String>) -> Self {
        self.user_explanation = Some(s.into());
        self
    }
}

/// Append one event as a single line to events.jsonl. Returns Err if the
/// serialized line would exceed MAX_EVENT_LINE_BYTES (truncation policy
/// is the caller's responsibility — typical fix is trimming `before`/`after`).
pub fn append(path: &Path, event: &Event) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create parent of {path:?}"))?;
    }
    let mut line = serde_json::to_string(event).context("serialize event")?;
    line.push('\n');
    if line.len() > MAX_EVENT_LINE_BYTES {
        anyhow::bail!(
            "event line {} bytes exceeds {} byte cap (atomicity guarantee broken)",
            line.len(),
            MAX_EVENT_LINE_BYTES
        );
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {path:?} for append"))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append to {path:?}"))?;
    Ok(())
}

/// Load every event from the file. Returns an empty Vec if the file
/// doesn't exist. A partial trailing line (interrupted writer) is dropped.
pub fn load(path: &Path) -> Result<Vec<Event>> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read {path:?}"))),
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Skip malformed lines silently — partial appends on crash, etc.
        if let Ok(ev) = serde_json::from_str::<Event>(line) {
            out.push(ev);
        }
    }
    Ok(out)
}
