//! Extract a surface's intent (overall + latest user ask) from the agent's
//! native transcript — the file cmux's binding registry points at
//! (`cmux_sessions::HookSession::transcript_path`).
//!
//! This is the per-surface, deterministic counterpart to the remote screen-grab
//! + LLM path: a bound LOCAL agent has a clean structured transcript, so we read
//! the actual user turns (first = overall, last = latest) with no LLM and no
//! workspace-level broadcast. Two formats:
//!
//! - **Claude Code** (`type:"user"`, `message.content`): string content is a
//!   user turn; list content (tool_result) is skipped; harness-injected context
//!   (`workspace_ref=…`, `<system-reminder>`, slash-command wrappers, resume
//!   banners) is filtered out so it isn't mistaken for a prompt.
//! - **Codex** (`type:"event_msg"`, `payload.type:"user_message"`): the
//!   `payload.message` is the user turn. `developer`/`agent_message`/tool events
//!   are ignored.

use std::path::Path;

use serde_json::Value;

use crate::mc_data::session_log::{summarize_user_turn, ConversationIntent};
use crate::mc_data::surface_kind::SurfaceKind;

/// All real user turns in the bound transcript, in order. Empty/unreadable → [].
pub fn user_turns(agent: SurfaceKind, path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match agent {
        SurfaceKind::Codex => codex_user_turns(&text),
        // Claude / OtherAgent use the Claude Code transcript shape.
        _ => claude_user_turns(&text),
    }
}

/// Read the bound transcript and return {overall = first real user turn,
/// latest = last real user turn}. Empty/unreadable → empty intent. The
/// `overall` here is the deterministic fallback; a richer LLM session summary
/// overrides it when available (see the overall-summary path in `tui::app`).
pub fn intent_from_transcript(agent: SurfaceKind, path: &Path) -> ConversationIntent {
    let users = user_turns(agent, path);
    ConversationIntent {
        overall_goal: users.first().and_then(|t| summarize_user_turn(t)),
        latest_ask: users.last().and_then(|t| summarize_user_turn(t)),
    }
}

/// Harness/system text that appears as a "user" turn but isn't a real prompt.
fn is_injected_claude(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("workspace_ref=")
        || t.starts_with("<system-reminder")
        || t.starts_with("<command-")
        || t.starts_with("<local-command")
        || t.starts_with("Caveat:")
        || t.starts_with("[Request interrupted")
        || t.starts_with("This session is being continued")
        || t.starts_with("<user-prompt-submit-hook")
}

fn claude_user_turns(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(o) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if o.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        // Real user prompts arrive as plain-string content. List content is
        // tool_result (skip). Filter harness-injected context.
        if let Some(s) = o
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
        {
            let s = s.trim();
            if !s.is_empty() && !is_injected_claude(s) {
                out.push(s.to_string());
            }
        }
    }
    out
}

fn codex_user_turns(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(o) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if o.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let payload = match o.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if payload.get("type").and_then(Value::as_str) != Some("user_message") {
            continue;
        }
        if let Some(msg) = payload.get("message").and_then(Value::as_str) {
            let msg = msg.trim();
            if !msg.is_empty() {
                out.push(msg.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_skips_injected_and_tool_results() {
        let jsonl = concat!(
            r#"{"type":"user","message":{"content":"workspace_ref=workspace:5\nworkspace_id=X"}}"#, "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"out"}]}}"#, "\n",
            r#"{"type":"assistant","message":{"content":"working on it"}}"#, "\n",
            r#"{"type":"user","message":{"content":"fix the auth bug"}}"#, "\n",
            r#"{"type":"user","message":{"content":"<system-reminder>noise</system-reminder>"}}"#, "\n",
            r#"{"type":"user","message":{"content":"now add a test"}}"#, "\n",
        );
        let turns = claude_user_turns(jsonl);
        assert_eq!(turns, vec!["fix the auth bug", "now add a test"]);
    }

    #[test]
    fn codex_extracts_user_messages_only() {
        let jsonl = concat!(
            r#"{"type":"session_meta","payload":{"id":"x"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[]}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"Continue the migration"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"on it"}}"#, "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"now write the README"}}"#, "\n",
        );
        let turns = codex_user_turns(jsonl);
        assert_eq!(turns, vec!["Continue the migration", "now write the README"]);
    }
}
