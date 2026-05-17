use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(name = "mission-control", about = "cmux workspace mission control")]
pub struct Config {
    /// Path to session history files
    #[arg(long, default_value_os_t = default_histories_dir())]
    pub histories_dir: PathBuf,

    /// Path to device identity file
    #[arg(long, default_value_os_t = default_device_file())]
    pub device_file: PathBuf,

    /// OpenAI API key (or set OPENAI_API_KEY env var)
    #[arg(long, env = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,

    /// OpenAI model to use for summarization
    #[arg(long, default_value = "gpt-5.0")]
    pub model: String,

    /// Tool call count threshold before triggering LLM summarization
    #[arg(long, default_value_t = 10)]
    pub summary_threshold: u32,

    /// cmux binary path
    #[arg(long, default_value = "cmux")]
    pub cmux_bin: String,
}

fn default_histories_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("agents/histories")
}

fn default_device_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("agents/.device")
}

pub const SUMMARIZE_PROMPT: &str = r#"You are summarizing an AI coding agent's session for a mission-control dashboard.

Given the session context below, produce:
1. TRAJECTORY: A single sentence describing what the session is working on and where it's at.
2. NEXT_STEPS: 3-5 concrete next actions as checkbox items.

Be extremely concise. No filler.

Session context:
{context}

Respond in exactly this format:
TRAJECTORY: <one sentence>
NEXT_STEPS:
- [ ] <step 1>
- [ ] <step 2>
- [ ] <step 3>
"#;
