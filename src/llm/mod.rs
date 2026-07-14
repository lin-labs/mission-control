pub mod codex;
pub mod learning;
pub mod log;
pub mod openai;
pub mod short_text;
pub mod surface_summary;
pub mod trajectory_regen;
pub mod typesafe;

use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Summary {
    pub trajectory: String,
    pub next_steps: Vec<String>,
}

#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, context: &str) -> Result<Summary>;

    /// Open-ended prompt → raw string response. Used by trajectory regeneration
    /// and surface summarization where the caller handles parsing.
    /// `system` is the stable/cacheable part; `user` is the fresh per-call part.
    async fn regenerate_trajectory(&self, system: &str, user: &str) -> Result<String>;
}

// ── Retry helper ─────────────────────────────────────────────────────────────

/// Run `op` with up to 3 retries (4 total attempts), using exponential backoff.
/// Retries only on transient errors (429, 5xx, timeout, connection reset).
/// Permanent failures (4xx auth) are not retried.
pub async fn with_retry<T, F, Fut>(op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let default_delays = [
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
    ];
    with_retry_delays(&default_delays, op).await
}

/// Like `with_retry` but with caller-supplied delay sequence (useful in tests
/// to avoid real sleeps).
pub async fn with_retry_delays<T, F, Fut>(delays: &[Duration], mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err: Option<anyhow::Error> = None;
    let total_attempts = delays.len() + 1;
    for attempt in 0..total_attempts {
        if attempt > 0 {
            tokio::time::sleep(delays[attempt - 1]).await;
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !is_transient_error(&e) {
                    return Err(e);
                }
                eprintln!(
                    "llm retry: attempt {} failed (transient): {e:?}",
                    attempt + 1
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("retry exhausted")))
}

fn is_transient_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("429")
        || msg.contains("500")
        || msg.contains("502")
        || msg.contains("503")
        || msg.contains("504")
        || msg.contains("timeout")
        || msg.contains("connection")
        || msg.contains("reset")
        || msg.contains("temporarily")
}
