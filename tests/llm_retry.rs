/// Tests for the LLM retry helper (T13).
///
/// Uses tiny delays to keep the test suite fast.

use mission_control::llm;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const FAST: &[Duration] = &[
    Duration::from_millis(1),
    Duration::from_millis(1),
    Duration::from_millis(1),
];

#[tokio::test]
async fn with_retry_succeeds_on_first_attempt() {
    let mut calls = 0usize;
    let result: anyhow::Result<i32> = llm::with_retry_delays(FAST, || {
        calls += 1;
        async move { Ok(42) }
    })
    .await;
    assert_eq!(result.unwrap(), 42);
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn with_retry_retries_on_transient_error() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    let result: anyhow::Result<i32> = llm::with_retry_delays(FAST, move || {
        let attempts = attempts_clone.clone();
        async move {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 3 {
                Err(anyhow::anyhow!("HTTP 503 service unavailable"))
            } else {
                Ok(100)
            }
        }
    })
    .await;
    assert_eq!(result.unwrap(), 100);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn with_retry_does_not_retry_on_permanent_error() {
    let mut attempts = 0usize;
    let result: anyhow::Result<()> = llm::with_retry_delays(FAST, || {
        attempts += 1;
        async move { Err(anyhow::anyhow!("HTTP 401 unauthorized")) }
    })
    .await;
    assert!(result.is_err());
    // Permanent error: only 1 attempt, no retries
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn with_retry_gives_up_after_4_attempts() {
    let mut attempts = 0usize;
    let result: anyhow::Result<()> = llm::with_retry_delays(FAST, || {
        attempts += 1;
        async move { Err(anyhow::anyhow!("HTTP 429 rate limit")) }
    })
    .await;
    assert!(result.is_err());
    // 1 initial + 3 retries (FAST has 3 delays) = 4 total
    assert_eq!(attempts, 4);
}
