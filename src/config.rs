use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Top-level CLI parser — subcommand optional; no subcommand launches the TUI.
#[derive(Parser, Debug, Clone)]
#[command(name = "mc", about = "cmux workspace mission control")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub tui: TuiConfig,
}

/// Subcommands available alongside the default TUI mode.
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Resolve a workspace UUID to its local data dir path.
    Resolve {
        /// The cmux workspace UUID.
        workspace_id: String,
    },
    /// One-time setup: create ~/data/mission-control/, print install summary.
    Setup,
    /// Promote ticked rules from a proposals file into the project's rules.md.
    PromoteRules {
        /// Path to the proposals .md file.
        proposals_file: PathBuf,
    },
    /// Bump the hit count for a matched prompt rule.
    RecordHit {
        /// The project the rule belongs to.
        project: String,
        /// Stable short hash of the rule's PATTERN.
        rule_id: String,
    },
    /// Garbage-collect stale rules across all projects' rules.md.
    Gc,
    /// Write a session-path pointer for the current surface.
    ///
    /// Typically invoked from an agent's SessionStart hook with env vars set
    /// (MC_WORKSPACE_ID, MC_SURFACE_ID). Becomes a silent no-op when env vars
    /// are not present, so it's safe to call unconditionally from a global hook.
    Bind {
        /// Surface ID; defaults to $MC_SURFACE_ID if env var is set.
        #[arg(env = "MC_SURFACE_ID")]
        surface_id: String,
        /// Path to the agent's session-history file. Falls back to
        /// $CLAUDE_SESSION_FILE, then auto-scans ~/agents/histories/.
        #[arg(long)]
        session_file: Option<PathBuf>,
    },
    /// One-shot cross-workspace daily summary.
    ///
    /// Builds digests for every visible cmux workspace (status, surfaces,
    /// trajectory.md Mission + Goals, recent commits), asks the configured
    /// summarizer (codex by default) for a qualitative report, and writes
    /// the result to
    /// `~obsAgents/mc-workspaces-summaries/YYYY-MM-DD-HH-summary.md`.
    /// The resulting path is printed to stdout on success; progress logs
    /// stream to stderr.
    Summarize,
    /// Backfill the registry JSON for the active cmux window and exit.
    BackfillWindow,
}

/// Configuration for the TUI (all existing flags live here).
#[derive(Args, Debug, Clone)]
pub struct TuiConfig {
    /// Path to session history files
    #[arg(long, default_value_os_t = default_histories_dir())]
    pub histories_dir: PathBuf,

    /// Path to device identity file
    #[arg(long, default_value_os_t = default_device_file())]
    pub device_file: PathBuf,

    /// OpenAI API key (or set OPENAI_API_KEY env var)
    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
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
    #[arg(long, env = "TYPESAFE_API_KEY", hide_env_values = true)]
    pub typesafe_api_key: Option<String>,

    /// xAI API key (or set XAI_API_KEY env var). Used for short one-shot
    /// generations like workspace goal-prefix codes (e.g. "MSC" for
    /// `mission-control`). Falls back to a deterministic algorithm when absent.
    #[arg(long, env = "XAI_API_KEY", hide_env_values = true)]
    pub xai_api_key: Option<String>,

    /// Codex CLI binary path (used for summarization via `codex exec`).
    /// Defaults to "codex" — looked up on PATH.
    #[arg(long, default_value = "codex")]
    pub codex_bin: String,

    /// Use `codex exec` for summarization instead of the OpenAI API.
    /// Defaults to true since Codex uses your local auth and is more reliable.
    #[arg(long, default_value_t = true)]
    pub use_codex: bool,
}

/// Backwards-compatibility alias so existing code referencing `Config` keeps working.
pub type Config = TuiConfig;

fn default_histories_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("agents/histories")
}

fn default_device_file() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join("agents/.device")
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
