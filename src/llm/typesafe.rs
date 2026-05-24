use super::log::{CallTimer, log_call};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Screen classification result from TypeSafe AI.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ScreenClassification {
    /// "working", "waiting", or "idle"
    pub state: String,
    pub state_confidence: f64,
    /// Whether an AI agent is visible (probability 0..1)
    pub has_agent: f64,
    /// Whether a user prompt is visible (probability 0..1)
    pub has_user_prompt: f64,
}

#[derive(Clone)]
pub struct TypeSafeClassifier {
    client: Client,
    api_key: String,
}

// --- Request types ---

#[derive(Serialize)]
struct EvalRequest {
    document: String,
    model: &'static str,
    prompts: Vec<serde_json::Value>,
}

// --- Response types ---

#[derive(Deserialize)]
struct EvalResponse {
    responses: Vec<serde_json::Value>,
}

impl TypeSafeClassifier {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
        }
    }

    /// Classify a terminal screen capture to determine agent state.
    /// Returns in sub-100ms — safe to call synchronously in the refresh loop.
    pub async fn classify_screen(&self, screen: &str) -> Result<ScreenClassification> {
        // Only the visible tail matters for state detection (spinner present?
        // empty prompt? activity line?). Trim to last 50 non-blank lines so we
        // don't ship 16k chars of scrollback every poll.
        let visible: String = {
            let lines: Vec<&str> = screen
                .lines()
                .map(|l| l.trim_end())
                .filter(|l| !l.is_empty())
                .collect();
            let start = lines.len().saturating_sub(50);
            lines[start..].join("\n")
        };
        let screen = &visible;

        let timer = CallTimer::start();
        let result = self.classify_inner(screen).await;

        let output_str = match &result {
            Ok(cls) => format!(
                "state={} conf={:.2} has_agent={:.2} has_user_prompt={:.2}",
                cls.state, cls.state_confidence, cls.has_agent, cls.has_user_prompt
            ),
            Err(_) => String::new(),
        };
        match &result {
            Ok(_) => log_call("typesafe", screen, Ok(&output_str), timer.ms()),
            Err(e) => log_call("typesafe", screen, Err(&format!("{:#}", e)), timer.ms()),
        }
        result
    }

    async fn classify_inner(&self, screen: &str) -> Result<ScreenClassification> {
        let request = EvalRequest {
            document: screen.to_string(),
            model: "speed_latest",
            prompts: vec![
                // 1. Agent state classification
                serde_json::json!({
                    "key": "agent_state",
                    "type": "choice",
                    "instructions": "What is the current state of the AI coding agent shown in this terminal screen capture?",
                    "options": [
                        {
                            "option": "working",
                            "description": "Agent is actively processing: spinner/animation visible, generating output, running commands, showing progress indicators, or displaying a thinking/loading state"
                        },
                        {
                            "option": "waiting",
                            "description": "Agent has finished its task and is showing an empty input prompt or cursor, waiting for the next human instruction. The conversation has ended and the prompt is ready for new input."
                        },
                        {
                            "option": "idle",
                            "description": "No AI coding agent is visible. This is a plain shell prompt, file editor, system monitor, or other non-agent terminal application."
                        }
                    ]
                }),
                // 2. Agent presence
                serde_json::json!({
                    "key": "has_agent",
                    "type": "noul",
                    "instructions": "Is an AI coding agent (such as Claude Code, Codex, Cursor, Aider, OpenCode, or similar AI programming assistant) visible or running in this terminal?"
                }),
                // 3. User prompt visibility
                serde_json::json!({
                    "key": "has_user_prompt",
                    "type": "noul",
                    "instructions": "Is there a visible message, instruction, or question from the human user to the AI agent in this terminal screen?"
                }),
            ],
        };

        let response = self
            .client
            .post("https://api.typesafe.ai/preview/evaluation")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await
            .context("TypeSafe API request failed")?
            .json::<EvalResponse>()
            .await
            .context("failed to parse TypeSafe response")?;

        // Parse the three responses
        let mut state = "idle".to_string();
        let mut state_confidence = 0.0;
        let mut has_agent = 0.0;
        let mut has_user_prompt = 0.0;

        for resp in &response.responses {
            let key = resp.get("key").and_then(|v| v.as_str()).unwrap_or("");
            match key {
                "agent_state" => {
                    state = resp
                        .get("chosen")
                        .and_then(|v| v.as_str())
                        .unwrap_or("idle")
                        .to_string();
                    state_confidence = resp
                        .get("confidence")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                }
                "has_agent" => {
                    has_agent = resp
                        .get("probability")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                }
                "has_user_prompt" => {
                    has_user_prompt = resp
                        .get("probability")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                }
                _ => {}
            }
        }

        Ok(ScreenClassification {
            state,
            state_confidence,
            has_agent,
            has_user_prompt,
        })
    }
}
