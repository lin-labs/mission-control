pub mod codex;
pub mod log;
pub mod openai;
pub mod surface_summary;
pub mod trajectory_regen;
pub mod typesafe;

use anyhow::Result;
use async_trait::async_trait;

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
    async fn regenerate_trajectory(&self, prompt: &str) -> Result<String>;
}
