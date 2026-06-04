/// Integration tests for the LLM learning extraction module.
///
/// These tests do NOT call a real LLM.  They exercise:
/// - `build_prompt` (pure string building)
/// - `extract_candidates_section` (string parsing)
/// - `produce_learning` (mocked summarizer)
use anyhow::Result;
use async_trait::async_trait;
use mission_control::llm::learning::{
    LearningInputs, build_prompt, extract_candidates_section, format_as_proposals_file,
    produce_learning,
};
use mission_control::llm::{Summarizer, Summary};
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

// ── Helpers ──────────────────────────────────────────────────────────────────

fn minimal_inputs() -> LearningInputs {
    LearningInputs {
        workspace_uuid: "uuid-test".to_string(),
        workspace_name: "test-ws".to_string(),
        project: "test-project".to_string(),
        duration: "2h 15m".to_string(),
        surfaces_summary: vec!["claude".to_string(), "shell".to_string()],
        final_trajectory: "## Mission\n- Build a widget\n\n## Beads\n- [x] Widget built\n".to_string(),
        history_snapshots: vec!["## Mission\n- Initial goal\n".to_string()],
        inputs: vec!["First user instruction".to_string()],
        events_jsonl: r#"{"ts":"2026-05-23T10:00:00Z","source":"user","kind":"check","section":"Beads","after":"Widget built"}"#.to_string(),
        session_history_files: vec![],
        shell_logs: vec![],
        surface_summaries: vec!["claude: building the widget".to_string()],
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn build_prompt_contains_trajectory_text() {
    let inputs = minimal_inputs();
    let prompt = build_prompt(&inputs);
    assert!(
        prompt.contains("Build a widget"),
        "prompt should contain trajectory content"
    );
}

#[test]
fn build_prompt_contains_workspace_name() {
    let inputs = minimal_inputs();
    let prompt = build_prompt(&inputs);
    assert!(
        prompt.contains("test-ws"),
        "prompt should contain workspace name"
    );
}

#[test]
fn build_prompt_contains_events() {
    let inputs = minimal_inputs();
    let prompt = build_prompt(&inputs);
    assert!(
        prompt.contains("Widget built"),
        "prompt should contain event content"
    );
}

#[test]
fn build_prompt_contains_history_snapshot() {
    let inputs = minimal_inputs();
    let prompt = build_prompt(&inputs);
    assert!(
        prompt.contains("Initial goal"),
        "prompt should contain history snapshot content"
    );
}

#[test]
fn build_prompt_contains_surface_summaries() {
    let inputs = minimal_inputs();
    let prompt = build_prompt(&inputs);
    assert!(
        prompt.contains("building the widget"),
        "prompt should contain surface summaries"
    );
}

#[test]
fn extract_candidates_section_returns_none_when_absent() {
    let response = "## Goal arc\n- did stuff\n\n## Outputs\n- some file\n";
    assert!(
        extract_candidates_section(response).is_none(),
        "should return None when section is absent"
    );
}

#[test]
fn extract_candidates_section_returns_content() {
    let response = concat!(
        "## Goal arc\n- built stuff\n\n",
        "## Prompt-optimization candidates\n",
        "- [ ] PATTERN: \"build a thing\"\n",
        "      EXPANSION: \"Use the widget approach\"\n",
        "      confidence: high\n",
        "      evidence: events 1-3\n",
    );
    let result = extract_candidates_section(response);
    assert!(result.is_some(), "should find the candidates section");
    let content = result.unwrap();
    assert!(
        content.contains("build a thing"),
        "should contain the pattern"
    );
    assert!(
        content.contains("widget approach"),
        "should contain the expansion"
    );
}

#[test]
fn extract_candidates_section_stops_at_next_heading() {
    let response = concat!(
        "## Prompt-optimization candidates\n",
        "- [ ] PATTERN: \"the trigger\"\n",
        "      EXPANSION: \"the rule\"\n",
        "\n",
        "## Other section\n",
        "- should not be included\n",
    );
    let content = extract_candidates_section(response).unwrap();
    assert!(
        !content.contains("should not be included"),
        "should not include content past next heading"
    );
    assert!(
        content.contains("the trigger"),
        "should contain the pattern"
    );
}

#[test]
fn format_as_proposals_file_has_preamble() {
    let candidates = "- [ ] PATTERN: \"foo\"\n      EXPANSION: \"bar\"\n";
    let result = format_as_proposals_file(candidates, "my-ws");
    assert!(
        result.contains("Tick the rules you want to promote"),
        "should contain the tick instructions"
    );
    assert!(
        result.contains("promote-rules"),
        "should reference promote-rules command"
    );
    assert!(result.contains("my-ws"), "should contain workspace name");
    assert!(result.contains("PATTERN:"), "should contain the candidates");
}

#[tokio::test]
async fn produce_learning_returns_full_record() {
    let full_response = concat!(
        "## Goal arc\n- Built widget [snap-1]\n\n",
        "## Final trajectory\n(trajectory content)\n\n",
        "## Key turns\n- Turned key [snap-1]\n\n",
        "## Surfaces\n- Claude: worked hard\n\n",
        "## Outputs\n- widget.rs created\n\n",
        "## Tooling & infra improvements\n- None\n\n",
        "## Skill recommendations\n- None\n\n",
        "## User prompt improvements\n- Be clearer\n\n",
        "## Prompt-optimization candidates\n",
        "- [ ] PATTERN: \"build widget\"\n",
        "      EXPANSION: \"Follow the widget pattern\"\n",
        "      confidence: med\n",
        "      evidence: event 3\n",
    );

    let summarizer: Arc<dyn Summarizer> = Arc::new(MockSummarizer {
        response: full_response.to_string(),
    });

    let inputs = minimal_inputs();
    let result = produce_learning(&summarizer, &inputs).await;
    assert!(
        result.is_ok(),
        "produce_learning should succeed: {:?}",
        result.err()
    );

    let outputs = result.unwrap();
    assert!(
        outputs.full_record_md.contains("Goal arc"),
        "full record should contain Goal arc section"
    );
    assert!(
        outputs
            .full_record_md
            .contains("Prompt-optimization candidates"),
        "full record should contain candidates section"
    );
    assert!(
        outputs.candidates_only_md.is_some(),
        "candidates_only_md should be Some when candidates section exists"
    );
    let proposals = outputs.candidates_only_md.unwrap();
    assert!(
        proposals.contains("build widget"),
        "proposals file should contain the candidate pattern"
    );
    assert!(
        proposals.contains("Tick the rules"),
        "proposals file should have the preamble"
    );
}

#[tokio::test]
async fn produce_learning_no_candidates_section() {
    let response_without_candidates = concat!(
        "## Goal arc\n- did stuff\n\n",
        "## Final trajectory\n(traj)\n\n",
        "## Key turns\n- some turn\n\n",
        "## Surfaces\n- surface info\n\n",
        "## Outputs\n- none\n\n",
        "## Tooling & infra improvements\n- none\n\n",
        "## Skill recommendations\n- none\n\n",
        "## User prompt improvements\n- none\n",
    );

    let summarizer: Arc<dyn Summarizer> = Arc::new(MockSummarizer {
        response: response_without_candidates.to_string(),
    });

    let inputs = minimal_inputs();
    let outputs = produce_learning(&summarizer, &inputs).await.unwrap();
    assert!(
        outputs.candidates_only_md.is_none(),
        "candidates_only_md should be None when no candidates section in response"
    );
}

#[tokio::test]
async fn produce_learning_llm_error_propagates() {
    struct FailingSummarizer;

    #[async_trait]
    impl Summarizer for FailingSummarizer {
        async fn summarize(&self, _: &str) -> Result<Summary> {
            anyhow::bail!("llm down")
        }
        async fn regenerate_trajectory(&self, _: &str, _: &str) -> Result<String> {
            anyhow::bail!("llm down")
        }
    }

    let summarizer: Arc<dyn Summarizer> = Arc::new(FailingSummarizer);
    let inputs = minimal_inputs();
    let result = produce_learning(&summarizer, &inputs).await;
    assert!(result.is_err(), "should propagate LLM error");
    let err = result.unwrap_err();
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("llm down"),
        "error message should be preserved: {msg}"
    );
}
