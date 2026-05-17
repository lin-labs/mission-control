use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub session_id: String,
    pub workspace_id: String,
    pub tool_name: Option<String>,
    pub event_name: String,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    payload: Option<RawPayload>,
    #[serde(rename = "type")]
    event_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    phase: Option<String>,
}

/// Spawn `cmux events --reconnect --category agent --no-heartbeat` and stream parsed events.
pub async fn subscribe(
    cmux_bin: &str,
    tx: mpsc::UnboundedSender<AgentEvent>,
) -> Result<()> {
    let mut child = Command::new(cmux_bin)
        .args([
            "events",
            "--reconnect",
            "--category",
            "agent",
            "--no-heartbeat",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout from cmux events"))?;

    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let raw: RawEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Skip ack frames and non-event types
        if raw.event_type.as_deref() != Some("event") {
            continue;
        }

        let payload = match raw.payload {
            Some(p) => p,
            None => continue,
        };

        // Only process completed hook events (not received+completed duplicates)
        if payload.phase.as_deref() != Some("completed") {
            continue;
        }

        let session_id = match payload.session_id {
            Some(id) => id,
            None => continue,
        };

        let workspace_id = match raw.workspace_id {
            Some(id) => id,
            None => continue,
        };

        let event = AgentEvent {
            session_id,
            workspace_id,
            tool_name: payload.tool_name,
            event_name: raw.name.unwrap_or_default(),
        };

        if tx.send(event).is_err() {
            break; // receiver dropped
        }
    }

    Ok(())
}
