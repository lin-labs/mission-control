use super::log::{log_call, CallTimer};
use super::{Summarizer, Summary};
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

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

    async fn regenerate_trajectory(&self, prompt: &str) -> Result<String> {
        let timer = CallTimer::start();

        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            max_tokens: 2048,
            temperature: 0.2,
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
            Ok(text) => log_call("openai-regen", prompt, Ok(text.as_str()), timer.ms()),
            Err(e) => log_call("openai-regen", prompt, Err(&format!("{:#}", e)), timer.ms()),
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
