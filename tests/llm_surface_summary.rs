/// Integration tests for surface summary prompt building and writing.

use anyhow::Result;
use async_trait::async_trait;
use mission_control::llm::{Summary, Summarizer};
use mission_control::llm::surface_summary::{summarize, write_summary_file, SurfaceSummaryInputs};
use std::sync::Arc;

// ── Mock Summarizer ──────────────────────────────────────────────────────────

struct MockSummarizer {
    response: String,
}

#[async_trait]
impl Summarizer for MockSummarizer {
    async fn summarize(&self, _context: &str) -> Result<Summary> {
        Ok(Summary {
            trajectory: "mock".to_string(),
            next_steps: vec![],
        })
    }

    async fn regenerate_trajectory(&self, _prompt: &str) -> Result<String> {
        Ok(self.response.clone())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn summarize_returns_non_empty_string() {
    let summarizer: Arc<dyn Summarizer> = Arc::new(MockSummarizer {
        response: "$ running cargo test".to_string(),
    });

    let inputs = SurfaceSummaryInputs {
        kind: "shell".to_string(),
        cwd: "~/projects/my-app".to_string(),
        recent_commands: vec![
            "1234\t0\t~/projects/my-app\tcargo test".to_string(),
        ],
    };

    let result = summarize(&summarizer, &inputs).await;
    assert!(result.is_ok(), "summarize should succeed");
    let text = result.unwrap();
    assert!(!text.is_empty(), "summary should not be empty");
    assert!(text.len() <= 80, "summary should be at most 80 chars");
}

#[tokio::test]
async fn summarize_truncates_to_80_chars() {
    let long_response = "a".repeat(200);
    let summarizer: Arc<dyn Summarizer> = Arc::new(MockSummarizer {
        response: long_response,
    });

    let inputs = SurfaceSummaryInputs {
        kind: "shell".to_string(),
        cwd: "/tmp".to_string(),
        recent_commands: vec![],
    };

    let result = summarize(&summarizer, &inputs).await;
    assert!(result.is_ok());
    assert!(result.unwrap().len() <= 80, "summary must be truncated to 80 chars");
}

#[tokio::test]
async fn summarize_picks_first_non_empty_line() {
    let summarizer: Arc<dyn Summarizer> = Arc::new(MockSummarizer {
        response: "\n\n$ git status\nsome trailing text".to_string(),
    });

    let inputs = SurfaceSummaryInputs {
        kind: "shell".to_string(),
        cwd: "/tmp".to_string(),
        recent_commands: vec![],
    };

    let result = summarize(&summarizer, &inputs).await;
    assert_eq!(result.unwrap(), "$ git status");
}

#[test]
fn write_summary_file_creates_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let surfaces_dir = dir.path();

    write_summary_file(surfaces_dir, "sid-abc", "$ running tests")
        .expect("write_summary_file should succeed");

    let path = surfaces_dir.join("sid-abc.summary");
    assert!(path.exists(), "summary file should be created");
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content, "$ running tests");
}

#[test]
fn write_summary_file_overwrites_existing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let surfaces_dir = dir.path();

    write_summary_file(surfaces_dir, "sid-xyz", "first summary").unwrap();
    write_summary_file(surfaces_dir, "sid-xyz", "second summary").unwrap();

    let content = std::fs::read_to_string(surfaces_dir.join("sid-xyz.summary")).unwrap();
    assert_eq!(content, "second summary", "should overwrite with newer summary");
}
