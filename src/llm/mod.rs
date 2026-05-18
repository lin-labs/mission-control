pub mod codex;
pub mod log;
pub mod openai;
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
}
