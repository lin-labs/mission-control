//! Reader for the centralized mux protocol session state.
//!
//! `~/data/mux/sessions/<session_id>.json` is written by the arcmux hook
//! CLI and is read-only for subscribers. Mission-control uses it for
//! per-agent-session activity facts; it does not write or mutate these files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Deserializer};

const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MuxSessionState {
    pub session_id: String,
    pub agent: String,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
    pub last_event: String,
    #[serde(default)]
    pub last_tool: Option<String>,
    pub working: bool,
    #[serde(default)]
    pub turn_count: u64,
    #[serde(default)]
    pub events_seen: u64,
    #[serde(default, deserialize_with = "deserialize_nonzero_timestamp")]
    pub last_prompt_submit_at: Option<DateTime<FixedOffset>>,
    #[serde(default, deserialize_with = "deserialize_nonzero_timestamp")]
    pub last_turn_end_at: Option<DateTime<FixedOffset>>,
}

impl MuxSessionState {
    pub fn has_ended_turn(&self) -> bool {
        self.last_turn_end_at.is_some() || self.last_event == "turn_end"
    }
}

pub fn session_state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("MC_MUX_SESSION_STATE_DIR") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join("data")
        .join("mux")
        .join("sessions")
}

pub fn load_session_in_dir(dir: &Path, session_id: &str) -> Result<Option<MuxSessionState>> {
    if !is_safe_session_id(session_id) {
        return Ok(None);
    }

    for path in [
        dir.join(format!("{session_id}.json")),
        dir.join("archived").join(format!("{session_id}.json")),
    ] {
        if path.is_file() {
            return read_session_file(&path).map(Some);
        }
    }

    Ok(None)
}

pub fn load_all_in_dir(dir: &Path) -> Vec<MuxSessionState> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            read_session_file(&path).ok()
        })
        .collect()
}

pub fn read_session_file(path: &Path) -> Result<MuxSessionState> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read mux session state {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parse mux session state {}", path.display()))
}

fn deserialize_nonzero_timestamp<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<DateTime<FixedOffset>>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == ZERO_TIME {
        return Ok(None);
    }
    DateTime::parse_from_rfc3339(trimmed)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && !session_id.contains('/')
        && !session_id.contains('\\')
        && !session_id.contains("..")
}
