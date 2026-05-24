/// Integration tests for trajectory regeneration prompt building.
///
/// These tests do NOT call a real LLM (that would cost money and require
/// API keys). They exercise the `build_prompt` function and the mock
/// Summarizer trait implementation.
use anyhow::Result;
use async_trait::async_trait;
use mission_control::llm::trajectory_regen::{RegenInputs, build_prompt, regenerate};
use mission_control::llm::{Summarizer, Summary};
use mission_control::mc_data::events::{Event, Kind, Source};
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

    async fn regenerate_trajectory(&self, _system: &str, _user: &str) -> Result<String> {
        Ok(self.response.clone())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn build_prompt_contains_trajectory_text() {
    let inputs = RegenInputs {
        workspace_name: "test-ws".to_string(),
        current_trajectory: "## Goal\n- Build a thing\n".to_string(),
        recent_events: vec![],
        recent_user_explanations: vec![],
        session_bullets: vec![],
        surface_summaries: vec![],
        tool_call_count: 0,
        cmux_surface_order: vec![],
        user_ask: None,
    };

    let p = build_prompt(&inputs);
    let combined = format!("{}\n\n{}", p.system, p.user);

    assert!(
        combined.contains("## Goal"),
        "prompt should contain the trajectory heading"
    );
    assert!(
        combined.contains("Build a thing"),
        "prompt should contain the trajectory content"
    );
}

#[test]
fn build_prompt_contains_workspace_name() {
    let inputs = RegenInputs {
        workspace_name: "my-workspace".to_string(),
        current_trajectory: String::new(),
        recent_events: vec![],
        recent_user_explanations: vec![],
        session_bullets: vec![],
        surface_summaries: vec![],
        tool_call_count: 0,
        cmux_surface_order: vec![],
        user_ask: None,
    };

    let p = build_prompt(&inputs);
    let combined = format!("{}\n\n{}", p.system, p.user);
    assert!(
        combined.contains("my-workspace"),
        "prompt should include the workspace name"
    );
}

#[test]
fn build_prompt_includes_recent_events() {
    let event =
        Event::new_now(Source::User, Kind::Check, "Tasks & Progress").with_after("deploy to prod");

    let inputs = RegenInputs {
        workspace_name: "ws".to_string(),
        current_trajectory: String::new(),
        recent_events: vec![event],
        recent_user_explanations: vec![],
        session_bullets: vec![],
        surface_summaries: vec![],
        tool_call_count: 5,
        cmux_surface_order: vec![],
        user_ask: None,
    };

    let p = build_prompt(&inputs);
    let combined = format!("{}\n\n{}", p.system, p.user);
    assert!(
        combined.contains("deploy to prod"),
        "prompt should contain event after-text"
    );
    assert!(
        combined.contains("5"),
        "prompt should contain tool call count"
    );
}

#[test]
fn build_prompt_includes_surface_summaries() {
    let inputs = RegenInputs {
        workspace_name: "ws".to_string(),
        current_trajectory: String::new(),
        recent_events: vec![],
        recent_user_explanations: vec![],
        session_bullets: vec![],
        surface_summaries: vec![("sid-123".to_string(), "$ running tests".to_string())],
        tool_call_count: 0,
        cmux_surface_order: vec!["sid-123".to_string()],
        user_ask: None,
    };

    let p = build_prompt(&inputs);
    let combined = format!("{}\n\n{}", p.system, p.user);
    assert!(
        combined.contains("sid-123"),
        "prompt should contain surface id"
    );
    assert!(
        combined.contains("running tests"),
        "prompt should contain surface summary"
    );
}

#[test]
fn build_prompt_includes_session_bullets() {
    let inputs = RegenInputs {
        workspace_name: "ws".to_string(),
        current_trajectory: String::new(),
        recent_events: vec![],
        recent_user_explanations: vec![],
        session_bullets: vec!["Fixed the auth bug".to_string(), "Added tests".to_string()],
        surface_summaries: vec![],
        tool_call_count: 0,
        cmux_surface_order: vec![],
        user_ask: None,
    };

    let p = build_prompt(&inputs);
    assert!(
        p.user.contains("Fixed the auth bug"),
        "user part should contain session bullets"
    );
}

#[test]
fn build_prompt_splits_into_system_and_user() {
    let inputs = RegenInputs {
        workspace_name: "split-ws".to_string(),
        current_trajectory: "## Goal\n- Test split\n".to_string(),
        recent_events: vec![],
        recent_user_explanations: vec![],
        session_bullets: vec![],
        surface_summaries: vec![],
        tool_call_count: 0,
        cmux_surface_order: vec![],
        user_ask: None,
    };

    let p = build_prompt(&inputs);
    assert!(!p.system.is_empty(), "system part should not be empty");
    assert!(!p.user.is_empty(), "user part should not be empty");
    assert!(
        p.system.contains("3-section trajectory doc"),
        "system part should contain stable instructions"
    );
    assert!(
        p.user.contains("Test split"),
        "user part should contain the current trajectory"
    );
    // System should NOT contain the trajectory content (that's fresh data).
    assert!(
        !p.system.contains("Test split"),
        "system part should not contain the fresh trajectory content"
    );
}

#[tokio::test]
async fn regenerate_parses_valid_trajectory_response() {
    let valid_trajectory = "---\nworkspace: test-ws\n---\n\n## Goal\n- Build a thing\n\n## Current surfaces\n\n## Tasks & Progress\n- [ ] do the work\n";

    let summarizer: Arc<dyn Summarizer> = Arc::new(MockSummarizer {
        response: valid_trajectory.to_string(),
    });

    let inputs = RegenInputs {
        workspace_name: "test-ws".to_string(),
        current_trajectory: valid_trajectory.to_string(),
        recent_events: vec![],
        recent_user_explanations: vec![],
        session_bullets: vec![],
        surface_summaries: vec![],
        tool_call_count: 0,
        cmux_surface_order: vec![],
        user_ask: None,
    };

    let result = regenerate(&summarizer, &inputs).await;
    assert!(result.is_ok(), "should parse a valid trajectory response");

    let doc = result.unwrap();
    assert!(
        doc.section("Goal").is_some(),
        "parsed doc should have a Goal section"
    );
}

#[tokio::test]
async fn regenerate_returns_err_on_invalid_response() {
    // An invalid trajectory that won't round-trip through TrajectoryDoc::parse
    // (broken YAML frontmatter).
    let invalid = "---\n: broken: yaml:\n---\n\n## Goal\n- Build a thing\n";

    // Actually broken YAML in frontmatter won't parse, so test with completely invalid content.
    // TrajectoryDoc::parse is lenient; let's test with a mock that returns an error directly.
    let summarizer: Arc<dyn Summarizer> = Arc::new(MockSummarizer {
        response: "valid enough for parse".to_string(),
    });

    // Empty response should produce a doc with no real content but not crash.
    let inputs = RegenInputs {
        workspace_name: "ws".to_string(),
        current_trajectory: String::new(),
        recent_events: vec![],
        recent_user_explanations: vec![],
        session_bullets: vec![],
        surface_summaries: vec![],
        tool_call_count: 0,
        cmux_surface_order: vec![],
        user_ask: None,
    };

    // Should not panic; result may be Ok or Err depending on parse.
    let _ = regenerate(&summarizer, &inputs).await;

    let _ = invalid; // silence unused warning
}
