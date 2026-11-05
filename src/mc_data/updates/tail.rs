//! Incremental transcript reader: reads only the bytes appended since `from`
//! and classifies turns as the user's own input vs agent-side.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::mc_data::surface_kind::SurfaceKind;
use crate::mc_data::transcript::is_injected_claude;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Reply,
    /// Peer, task-notification, coordinator, letters, injected context.
    Injected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    User {
        at: DateTime<Utc>,
        text: String,
    },
    Agent {
        at: DateTime<Utc>,
        text: String,
        kind: AgentKind,
    },
}

#[derive(Debug)]
pub struct ReadOutcome {
    pub turns: Vec<Turn>,
    /// Every parsed line, so callers can scan tool calls.
    pub lines: Vec<Value>,
    pub next_offset: u64,
    /// `from` was past the end of the file; it was read from 0.
    pub reset: bool,
}

/// Read complete lines from `from`. A trailing partial line is left for the
/// next call; unparseable lines are skipped but advance the offset.
/// Lines without a valid `timestamp` get `fallback_at` (the file mtime).
pub fn read_new(
    agent: SurfaceKind,
    path: &Path,
    from: u64,
    fallback_at: DateTime<Utc>,
) -> std::io::Result<ReadOutcome> {
    let mut file = std::fs::File::open(path)?;
    let reset = from > file.metadata()?.len();
    let from = if reset { 0 } else { from };
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);

    let (mut turns, mut lines) = (Vec::new(), Vec::new());
    for raw in buf[..end].split(|&b| b == b'\n') {
        let Ok(line) = serde_json::from_slice::<Value>(raw) else {
            continue;
        };
        let at = line
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map_or(fallback_at, |t| t.with_timezone(&Utc));
        let turn = match agent {
            SurfaceKind::Codex => codex_turn(&line, at),
            _ => claude_turn(&line, at),
        };
        turns.extend(turn);
        lines.push(line);
    }
    Ok(ReadOutcome {
        turns,
        lines,
        next_offset: from + end as u64,
        reset,
    })
}

fn claude_turn(line: &Value, at: DateTime<Utc>) -> Option<Turn> {
    let content = line.get("message")?.get("content")?;
    match line.get("type")?.as_str()? {
        "assistant" => {
            let text = content_text(content)?;
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(Turn::Agent {
                at,
                text,
                kind: AgentKind::Reply,
            })
        }
        "user" => {
            let text = content_text(content)?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let origin = line.pointer("/origin/kind").and_then(Value::as_str);
            let injected = match origin {
                Some("human") => text.starts_with("Protocol: blin-letter/1"),
                Some(_) => true,
                None => is_injected_claude(&text),
            };
            Some(if injected {
                Turn::Agent {
                    at,
                    text,
                    kind: AgentKind::Injected,
                }
            } else {
                Turn::User { at, text }
            })
        }
        _ => None,
    }
}

/// String content, or the `text` blocks of list content joined with newlines.
fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn codex_turn(line: &Value, at: DateTime<Utc>) -> Option<Turn> {
    if line.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let p = line.get("payload")?;
    let text = p.get("message")?.as_str()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    match p.get("type")?.as_str()? {
        "user_message" => Some(Turn::User { at, text }),
        "agent_message" => Some(Turn::Agent {
            at,
            text,
            kind: AgentKind::Reply,
        }),
        _ => None,
    }
}
