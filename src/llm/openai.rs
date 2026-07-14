use super::log::{CallTimer, log_call};
use super::{Summarizer, Summary};
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::process::Command;

const GCP_PROJECT: &str = "reflectionai";
const GCP_SECRET: &str = "OPENAI_API_KEY_AGENTIC";
const GCP_SECRET_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyResolution {
    pub api_key: Option<String>,
    pub warning: Option<String>,
}

/// Resolve an OpenAI key without persisting or logging it. Explicit CLI/env
/// configuration wins; otherwise use the machine's gcloud authentication.
pub async fn resolve_api_key(explicit: Option<String>) -> ApiKeyResolution {
    let args = [
        "secrets",
        "versions",
        "access",
        "latest",
        "--secret",
        GCP_SECRET,
        "--project",
        GCP_PROJECT,
    ];
    resolve_api_key_with_command(explicit, "gcloud", &args, GCP_SECRET_TIMEOUT).await
}

async fn resolve_api_key_with_command(
    explicit: Option<String>,
    binary: &str,
    args: &[&str],
    timeout: Duration,
) -> ApiKeyResolution {
    if let Some(key) = explicit
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        return ApiKeyResolution {
            api_key: Some(key),
            warning: None,
        };
    }

    let mut command = Command::new(binary);
    command.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, command.output()).await {
        Err(_) => ApiKeyResolution {
            api_key: None,
            warning: Some(
                "OpenAI key unavailable: gcloud secret lookup timed out; authenticate gcloud and reload mc"
                    .to_string(),
            ),
        },
        Ok(Err(_)) => ApiKeyResolution {
            api_key: None,
            warning: Some(
                "OpenAI key unavailable: could not run gcloud; install/authenticate gcloud and reload mc"
                    .to_string(),
            ),
        },
        Ok(Ok(output)) if !output.status.success() => ApiKeyResolution {
            api_key: None,
            warning: Some(format!(
                "OpenAI key unavailable: gcloud secret lookup failed (exit {}); authenticate gcloud and reload mc",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            )),
        },
        Ok(Ok(output)) => {
            let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if key.is_empty() {
                ApiKeyResolution {
                    api_key: None,
                    warning: Some(
                        "OpenAI key unavailable: gcloud secret lookup returned an empty value; reload mc after fixing access"
                            .to_string(),
                    ),
                }
            } else {
                ApiKeyResolution {
                    api_key: Some(key),
                    warning: None,
                }
            }
        }
    }
}

pub struct OpenAISummarizer {
    client: Client,
    api_key: String,
    model: String,
    prompt_template: String,
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

impl OpenAISummarizer {
    pub fn new(api_key: String, model: String, prompt_template: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
            prompt_template,
        }
    }

    fn build_prompt(&self, context: &str) -> String {
        self.prompt_template.replace("{context}", context)
    }
}

#[async_trait]
impl Summarizer for OpenAISummarizer {
    async fn summarize(&self, context: &str) -> Result<Summary> {
        let prompt = self.build_prompt(context);
        let timer = CallTimer::start();

        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message {
                role: "user".to_string(),
                content: prompt.clone(),
            }],
            max_tokens: 512,
            temperature: 0.3,
        };

        let result: Result<String> = async {
            let response = self
                .client
                .post("https://api.openai.com/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", self.api_key))
                .json(&request)
                .send()
                .await
                .context("OpenAI API request failed")?
                .json::<ChatResponse>()
                .await
                .context("failed to parse OpenAI response")?;

            Ok(response
                .choices
                .first()
                .map(|c| c.message.content.clone())
                .unwrap_or_default())
        }
        .await;

        match &result {
            Ok(text) => log_call("openai", &prompt, Ok(text.as_str()), timer.ms()),
            Err(e) => log_call("openai", &prompt, Err(&format!("{:#}", e)), timer.ms()),
        }

        let text = result?;
        parse_summary(&text)
    }

    async fn regenerate_trajectory(&self, system: &str, user: &str) -> Result<String> {
        let timer = CallTimer::start();
        let log_prompt = format!("[system]\n{system}\n\n[user]\n{user}");

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let system = system.to_string();
        let user = user.to_string();

        let result = crate::llm::with_retry(move || {
            let client = client.clone();
            let api_key = api_key.clone();
            let model = model.clone();
            let system = system.clone();
            let user = user.clone();
            async move {
                // Build request body with cache_control on the system message.
                let body = json!({
                    "model": model,
                    "messages": [
                        {
                            "role": "system",
                            "content": [
                                {
                                    "type": "text",
                                    "text": system,
                                    "cache_control": { "type": "ephemeral" }
                                }
                            ]
                        },
                        {
                            "role": "user",
                            "content": user
                        }
                    ],
                    "max_tokens": 2048,
                    "temperature": 0.2
                });

                let response: ChatResponse = client
                    .post("https://api.openai.com/v1/chat/completions")
                    .header("Authorization", format!("Bearer {api_key}"))
                    .json(&body)
                    .send()
                    .await
                    .context("OpenAI API request failed")?
                    .json::<ChatResponse>()
                    .await
                    .context("failed to parse OpenAI response")?;

                Ok(response
                    .choices
                    .first()
                    .map(|c| c.message.content.clone())
                    .unwrap_or_default())
            }
        })
        .await;

        match &result {
            Ok(text) => log_call("openai-regen", &log_prompt, Ok(text.as_str()), timer.ms()),
            Err(e) => log_call(
                "openai-regen",
                &log_prompt,
                Err(&format!("{:#}", e)),
                timer.ms(),
            ),
        }

        result
    }
}

fn parse_summary(text: &str) -> Result<Summary> {
    let mut trajectory = String::new();
    let mut next_steps = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("TRAJECTORY:") {
            trajectory = rest.trim().to_string();
        } else if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") {
            next_steps.push(trimmed.to_string());
        }
    }

    if trajectory.is_empty() {
        trajectory = "Summary unavailable".to_string();
    }

    Ok(Summary {
        trajectory,
        next_steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_key_skips_external_lookup() {
        let result = resolve_api_key_with_command(
            Some("  explicit-key\n".into()),
            "/definitely/missing/gcloud",
            &[],
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result.api_key.as_deref(), Some("explicit-key"));
        assert_eq!(result.warning, None);
    }

    #[tokio::test]
    async fn successful_lookup_trims_key() {
        let result = resolve_api_key_with_command(
            None,
            "/bin/sh",
            &["-c", "printf 'secret-from-gcloud\\n'"],
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.api_key.as_deref(), Some("secret-from-gcloud"));
        assert_eq!(result.warning, None);
    }

    #[tokio::test]
    async fn failed_lookup_is_sanitized() {
        let result = resolve_api_key_with_command(
            None,
            "/bin/sh",
            &["-c", "printf 'sensitive stderr' >&2; exit 7"],
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.api_key, None);
        let warning = result.warning.unwrap();
        assert!(warning.contains("exit 7"));
        assert!(!warning.contains("sensitive"));
    }

    #[tokio::test]
    async fn empty_lookup_is_non_fatal() {
        let result = resolve_api_key_with_command(
            None,
            "/bin/sh",
            &["-c", "printf '  \n'"],
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.api_key, None);
        assert!(result.warning.unwrap().contains("empty value"));
    }

    #[tokio::test]
    async fn timed_out_lookup_is_non_fatal() {
        let result = resolve_api_key_with_command(
            None,
            "/bin/sh",
            &["-c", "sleep 1"],
            Duration::from_millis(5),
        )
        .await;
        assert_eq!(result.api_key, None);
        assert!(result.warning.unwrap().contains("timed out"));
    }
}
