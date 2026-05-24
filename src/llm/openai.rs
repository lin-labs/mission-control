use super::log::{log_call, CallTimer};
use super::{Summarizer, Summary};
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;

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
            Err(e) => log_call("openai-regen", &log_prompt, Err(&format!("{:#}", e)), timer.ms()),
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
