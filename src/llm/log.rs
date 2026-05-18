use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

/// Global lock to prevent concurrent writes from interleaving JSON lines.
/// Without this, parallel TypeSafe + Codex tasks can corrupt the file.
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Directory for LLM call logs.
pub fn log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/mission-control/logs")
}

pub fn log_path() -> PathBuf {
    log_dir().join("llm.jsonl")
}

/// Append a single JSON line to the LLM call log.
/// `service` is e.g. "codex", "openai", "typesafe".
/// `result` is Ok(output) or Err(error_message).
pub fn log_call(
    service: &str,
    input: &str,
    result: Result<&str, &str>,
    duration_ms: u128,
) {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let ts = chrono::Utc::now().to_rfc3339();
    let entry = match result {
        Ok(output) => json!({
            "ts": ts,
            "service": service,
            "duration_ms": duration_ms,
            "input": input,
            "output": output,
        }),
        Err(err) => json!({
            "ts": ts,
            "service": service,
            "duration_ms": duration_ms,
            "input": input,
            "error": err,
        }),
    };

    // Lock + serialize to bytes first, then a single write_all under the lock
    let line = format!("{}\n", entry);
    let _guard = LOG_LOCK.lock().ok();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Stopwatch helper — returns elapsed milliseconds since the call.
pub struct CallTimer {
    start: Instant,
}

impl CallTimer {
    pub fn start() -> Self {
        Self { start: Instant::now() }
    }
    pub fn ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }
}
