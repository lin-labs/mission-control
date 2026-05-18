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

    /// cmux socket path (auto-detected from CMUX_SOCKET_PATH or default location)
    #[arg(long, env = "CMUX_SOCKET_PATH", default_value_os_t = default_socket_path())]
    pub cmux_socket: PathBuf,

    /// TypeSafe AI API key for screen classification (or set TYPESAFE_API_KEY env var)
    #[arg(long, env = "TYPESAFE_API_KEY")]
    pub typesafe_api_key: Option<String>,

    /// Codex CLI binary path (used for summarization via `codex exec`).
    /// Defaults to "codex" — looked up on PATH.
    #[arg(long, default_value = "codex")]
    pub codex_bin: String,

    /// Use `codex exec` for summarization instead of the OpenAI API.
    /// Defaults to true since Codex uses your local auth and is more reliable.
    #[arg(long, default_value_t = true)]
    pub use_codex: bool,
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

fn default_socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Application Support/cmux/cmux.sock")
}

pub const SUMMARIZE_PROMPT: &str = r#"You are summarizing one workspace inside a mission-control dashboard that watches many AI coding agent sessions in parallel. The user runs many of these at once and needs a chess-position summary: what was I asking, what has the agent done, and what should I focus on next.

The context below may contain a workspace name, the user's manual notes, a session activity log, and a verbatim terminal scrollback (the actual conversation between the human and the agent). Lines starting with `›` or `❯` are the human's messages; lines starting with `⏺`, `●`, `✻`, `✢`, `·`, `•`, or with `⎿` are the agent's output and tool calls.

Read the WHOLE context. Identify the most recent task the human asked about, what the agent has done so far on it, and what state things are currently in.

Produce:
1. TRAJECTORY: One sentence describing the current mission and where it stands. Be specific — mention what's being built/changed and the current state (e.g. "waiting on tests", "drafting PR", "blocked on X"). Do not describe the dashboard itself or generic phrases like "the session is summarizing".
2. NEXT_STEPS: 3-5 concrete next actions the HUMAN should take or watch for. Phrase from the human's perspective.

If the context is mostly empty or only shows a shell prompt, say so plainly in TRAJECTORY (e.g. "Idle — no agent running") and leave NEXT_STEPS empty.

Context:
{context}

Respond in exactly this format and nothing else:
TRAJECTORY: <one sentence>
NEXT_STEPS:
- [ ] <step 1>
- [ ] <step 2>
- [ ] <step 3>
"#;
