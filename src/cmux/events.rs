use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub session_id: String,
    pub workspace_id: String,
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
    phase: Option<String>,
}

/// The exact argv we hand to `cmux` for the agent event stream. Kept as one
/// slice so the spawn and the orphan-reaper match on identical text.
const EVENTS_ARGS: [&str; 5] = [
    "events",
    "--reconnect",
    "--category",
    "agent",
    "--no-heartbeat",
];

/// Kill any `cmux events …` subscribers left behind by a previous `mc` run.
///
/// The subscriber is a long-lived child; if `mc` is SIGKILLed (or crashes)
/// its destructors never run, so the subprocess reparents to launchd/init and
/// lingers. Each restart would otherwise stack another one. We reap by full
/// command-line match (`pkill -f`) on the exact arg string, which is unique to
/// this subscriber, *before* spawning ours so we never kill the new child.
/// Best-effort: a missing `pkill` or "no matches" exit is not an error.
fn reap_orphan_subscribers() {
    let pattern = EVENTS_ARGS.join(" ");
    let _ = std::process::Command::new("pkill")
        .arg("-f")
        .arg("--")
        .arg(&pattern)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Spawn `cmux events --reconnect --category agent --no-heartbeat` and stream parsed events.
pub async fn subscribe(
    cmux_bin: &str,
    socket_path: &std::path::Path,
    tx: mpsc::UnboundedSender<AgentEvent>,
) -> Result<()> {
    // Clear out any subscribers orphaned by a prior run before adding our own.
    reap_orphan_subscribers();

    let mut child = Command::new(cmux_bin)
        .args(EVENTS_ARGS)
        .env("CMUX_SOCKET_PATH", socket_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Ensure a graceful `mc` shutdown (task drop) takes the child with it,
        // so we don't become the orphan the next run has to reap.
        .kill_on_drop(true)
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
            event_name: raw.name.unwrap_or_default(),
        };

        if tx.send(event).is_err() {
            break; // receiver dropped
        }
    }

    Ok(())
}
