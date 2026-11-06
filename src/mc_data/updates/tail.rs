//! Incremental transcript reader: reads only the bytes appended since `from`
//! and classifies turns as the user's own input vs agent-side.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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
    /// Every parsed line, so callers can scan tool calls; only its
    /// `timestamp` and `message` are kept, bounding memory.
    pub lines: Vec<Value>,
    pub next_offset: u64,
    /// `from` was past the end of the file; it was read from 0.
    pub reset: bool,
}

/// Read complete lines from `from`, at most `max_bytes` (> 0) of them. A
/// trailing partial line is left for the next call; unparseable lines are
/// skipped but advance the offset. A line longer than `max_bytes` is skipped
/// unbuffered once complete. Lines without a valid `timestamp` get
/// `fallback_at` (the file mtime).
pub fn read_new(
    agent: SurfaceKind,
    path: &Path,
    from: u64,
    fallback_at: DateTime<Utc>,
    max_bytes: u64,
) -> std::io::Result<ReadOutcome> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let reset = from > len;
    let from = if reset { 0 } else { from };
    file.seek(SeekFrom::Start(from))?;
    // Sized up front: growing by doubling would peak at twice the window.
    let mut buf = Vec::with_capacity((len - from).min(max_bytes) as usize + 1);
    (&mut file).take(max_bytes).read_to_end(&mut buf)?;
    let end = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None if buf.len() as u64 == max_bytes => {
            drop(buf);
            let past = skip_line(file)?.map_or(0, |n| max_bytes + n);
            return Ok(ReadOutcome {
                turns: vec![],
                lines: vec![],
                next_offset: from + past,
                reset,
            });
        }
        None => 0,
    };

    let (mut turns, mut lines) = (Vec::new(), Vec::new());
    for raw in buf[..end].split(|&b| b == b'\n') {
        let Ok(mut line) = serde_json::from_slice::<Value>(raw) else {
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
        if let Value::Object(m) = &mut line {
            m.retain(|k, _| k == "timestamp" || k == "message");
        }
        lines.push(line);
    }
    Ok(ReadOutcome {
        turns,
        lines,
        next_offset: from + end as u64,
        reset,
    })
}

/// First line start at or after `pos`: `pos` itself when it follows a
/// newline, else past the next newline (the file length if there is none).
pub fn line_start_at(path: &Path, pos: u64) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if pos == 0 || pos > len {
        return Ok(pos.min(len));
    }
    file.seek(SeekFrom::Start(pos - 1))?;
    Ok(skip_line(file)?.map_or(len, |n| pos - 1 + n))
}

/// Bytes up to and including the next newline, streamed; `None` at EOF.
fn skip_line(file: std::fs::File) -> std::io::Result<Option<u64>> {
    let mut r = BufReader::new(file);
    let mut n = 0;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok(None);
        }
        if let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            return Ok(Some(n + i as u64 + 1));
        }
        let l = chunk.len();
        n += l as u64;
        r.consume(l);
    }
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
                // Scheduled/SDK runs are automation, not the user typing.
                None => {
                    is_injected_claude(&text)
                        || line["promptSource"] == "sdk"
                        || line["entrypoint"] == "sdk-cli"
                }
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
