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
    /// The arcmux turn contract: the agent-authored goal artifacts refreshed
    /// every turn (overall goal, current goal, progress path, success check,
    /// last user turn, vault link). Authoritative — written by the agent's
    /// hook, not inferred. Absent for sessions without a contract yet.
    #[serde(default)]
    pub turn_contract: Option<TurnContract>,
}

/// The compact, current per-session contract arcmux records each turn: what the
/// agent is doing now, the whole-conversation objective, how success is
/// verified, and the consolidated path taken. A snapshot, not a log. Every
/// field is optional; new fields arcmux adds deserialize without a code change.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct TurnContract {
    /// The latest gauged "Your ask:" — the current sub-task.
    #[serde(default)]
    pub goal: Option<String>,
    /// The whole-conversation objective (summarizer-refreshed).
    #[serde(default)]
    pub overall_goal: Option<String>,
    /// The raw, verbatim last user turn (truncated upstream).
    #[serde(default)]
    pub last_user_message: Option<String>,
    /// Where the conversation is saved in the vault.
    #[serde(default)]
    pub vault_link: Option<String>,
    /// Current concrete success check (validation).
    #[serde(default)]
    pub success_verification: Option<String>,
    /// Consolidated path taken/planned (progress).
    #[serde(default)]
    pub path: Option<String>,
    /// Which native event supplied the recording (UserPromptSubmit, Stop, …).
    #[serde(default)]
    pub source: Option<String>,
}

impl TurnContract {
    /// Trimmed value of a field, or None when empty/whitespace.
    fn field(value: &Option<String>) -> Option<&str> {
        value.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    pub fn goal(&self) -> Option<&str> {
        Self::field(&self.goal)
    }
    pub fn overall_goal(&self) -> Option<&str> {
        Self::field(&self.overall_goal)
    }
    pub fn last_user_message(&self) -> Option<&str> {
        Self::field(&self.last_user_message)
    }
    pub fn vault_link(&self) -> Option<&str> {
        Self::field(&self.vault_link)
    }
    pub fn success_verification(&self) -> Option<&str> {
        Self::field(&self.success_verification)
    }
    pub fn path(&self) -> Option<&str> {
        Self::field(&self.path)
    }

    /// The vault link reduced to its file name (the saved conversation log).
    pub fn vault_log_name(&self) -> Option<&str> {
        self.vault_link()
            .map(|v| v.rsplit(['/', '\\']).next().unwrap_or(v))
    }

    /// True when there is at least one artifact worth showing.
    pub fn has_content(&self) -> bool {
        self.goal().is_some()
            || self.overall_goal().is_some()
            || self.path().is_some()
            || self.success_verification().is_some()
            || self.last_user_message().is_some()
    }
}

impl MuxSessionState {
    pub fn has_ended_turn(&self) -> bool {
        self.last_turn_end_at.is_some() || self.last_event == "turn_end"
    }

    /// The turn contract, only when it carries something worth displaying.
    pub fn contract(&self) -> Option<&TurnContract> {
        self.turn_contract.as_ref().filter(|c| c.has_content())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> MuxSessionState {
        serde_json::from_str(json).expect("parse session state")
    }

    #[test]
    fn parses_turn_contract_artifacts() {
        let st = parse(
            r#"{
                "session_id": "s-1", "agent": "claude",
                "created_at": "2026-06-25T08:00:00-07:00",
                "updated_at": "2026-06-25T08:05:00-07:00",
                "last_event": "turn_end", "working": false,
                "turn_contract": {
                    "goal": "  ship the band  ",
                    "overall_goal": "adapt mc to arcmux contract",
                    "path": "added parser then band",
                    "success_verification": "cargo test green",
                    "last_user_message": "make mc adapt\nto this",
                    "vault_link": "/Users/blin/agents/histories/2026-06-25-08-mc.md"
                }
            }"#,
        );
        let c = st.contract().expect("contract present");
        assert_eq!(c.goal(), Some("ship the band")); // trimmed
        assert_eq!(c.overall_goal(), Some("adapt mc to arcmux contract"));
        assert_eq!(c.path(), Some("added parser then band"));
        assert_eq!(c.success_verification(), Some("cargo test green"));
        assert_eq!(c.vault_log_name(), Some("2026-06-25-08-mc.md"));
        assert!(c.has_content());
    }

    #[test]
    fn absent_or_empty_contract_is_none() {
        let no_field = parse(
            r#"{"session_id":"s-2","agent":"codex",
                "created_at":"2026-06-25T08:00:00-07:00",
                "updated_at":"2026-06-25T08:00:00-07:00",
                "last_event":"tool_start","working":true}"#,
        );
        assert!(no_field.contract().is_none());

        let blank = parse(
            r#"{"session_id":"s-3","agent":"codex",
                "created_at":"2026-06-25T08:00:00-07:00",
                "updated_at":"2026-06-25T08:00:00-07:00",
                "last_event":"tool_start","working":true,
                "turn_contract":{"goal":"   ","source":"Stop"}}"#,
        );
        assert!(blank.contract().is_none(), "whitespace-only goal is not content");
    }
}
