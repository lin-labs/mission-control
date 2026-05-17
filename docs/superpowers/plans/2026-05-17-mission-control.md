# Mission Control Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust TUI that monitors all cmux workspaces, shows agent session status/trajectory/next-steps via LLM summarization, and enables fast context-switching.

**Architecture:** Event-driven Rust app using ratatui for TUI, tokio for async. Three data sources: cmux CLI for workspace structure, cmux events stream for real-time session tracking, session markdown files (iCloud-synced) as canonical state. OpenAI gpt-5.0 for summarization behind an abstract trait. Sidebar+detail split layout.

**Tech Stack:** Rust 1.94, ratatui 0.30, crossterm 0.29, tokio, reqwest, serde, notify, clap

---

## File Structure

```
~/Tools/mission-control/
├── Cargo.toml
├── src/
│   ├── main.rs              # tokio entry, terminal setup/teardown, main event loop
│   ├── config.rs            # Config struct, paths, thresholds, CLI args via clap
│   ├── cmux/
│   │   ├── mod.rs           # re-exports
│   │   ├── client.rs        # async fns: list_workspaces, tree, read_screen (shells out to cmux)
│   │   └── events.rs        # CmuxEventStream: spawns `cmux events --reconnect`, parses NDJSON
│   ├── session/
│   │   ├── mod.rs           # re-exports
│   │   ├── file.rs          # SessionFile: parse/serialize markdown with YAML frontmatter
│   │   └── watcher.rs       # SessionWatcher: fsnotify on ~/agents/histories/, sends events via channel
│   ├── llm/
│   │   ├── mod.rs           # Summarizer trait + Summary struct
│   │   └── openai.rs        # OpenAISummarizer: reqwest calls to gpt-5.0
│   └── tui/
│       ├── mod.rs           # re-exports
│       ├── app.rs           # App state: workspaces vec, selected index, session data, screen cache
│       ├── sidebar.rs       # render_sidebar() — workspace list with status dots
│       └── detail.rs        # render_detail() — header, trajectory, bullets, next steps, screen preview
```

Hook (separate repo):
```
~/Projects/agents/configs/
├── claude/hooks/mission-control-hook.sh
├── codex/hooks/mission-control-hook.sh   (symlink to claude's)
```

## Dependency Graph (for parallelization)

Tasks 1-5 are independent leaf modules with no cross-dependencies. Task 6 (app.rs) depends on all of them. Task 7 (TUI widgets) depends on Task 6. Task 8 (main.rs) depends on everything. Task 9 (hook) is fully independent.

```
T1:config  T2:cmux/client  T3:cmux/events  T4:session/file  T5:llm  T9:hook
    \           \               |              /            /
     \           \              |             /            /
      +-----------+------  T6:app  ---------+------------+
                               |
                          T7:tui widgets
                               |
                          T8:main.rs
```

**Parallelizable groups:**
- Group A: T1, T2, T3, T4, T5, T9 (all independent)
- Group B: T6 (depends on A)
- Group C: T7 (depends on B)
- Group D: T8 (depends on C)

---

### Task 1: Project Scaffold + Config

**Files:**
- Create: `Cargo.toml`
- Create: `src/config.rs`
- Create: `src/main.rs` (stub)
- Create: `src/cmux/mod.rs` (stub)
- Create: `src/session/mod.rs` (stub)
- Create: `src/llm/mod.rs` (stub)
- Create: `src/tui/mod.rs` (stub)

- [ ] **Step 1: Initialize cargo project**

```bash
cd ~/Tools/mission-control
cargo init --name mission-control
```

- [ ] **Step 2: Write Cargo.toml**

```toml
[package]
name = "mission-control"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio = { version = "1", features = ["full"] }
ratatui = "0.30"
crossterm = "0.29"
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
notify = "8"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
async-trait = "0.1"
tokio-stream = "0.1"
chrono = { version = "0.4", features = ["serde"] }
dirs = "6"
```

- [ ] **Step 3: Write config.rs**

```rust
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
```

- [ ] **Step 4: Write stub main.rs and module files**

`src/main.rs`:
```rust
mod config;
mod cmux;
mod session;
mod llm;
mod tui;

fn main() {
    println!("mission-control stub");
}
```

`src/cmux/mod.rs`:
```rust
pub mod client;
pub mod events;
```

`src/session/mod.rs`:
```rust
pub mod file;
pub mod watcher;
```

`src/llm/mod.rs`:
```rust
pub mod openai;
```

`src/tui/mod.rs`:
```rust
pub mod app;
pub mod sidebar;
pub mod detail;
```

Create all stub files so the module tree compiles:
```rust
// src/cmux/client.rs, src/cmux/events.rs, src/session/file.rs,
// src/session/watcher.rs, src/llm/openai.rs, src/tui/app.rs,
// src/tui/sidebar.rs, src/tui/detail.rs
// Each file is initially empty.
```

- [ ] **Step 5: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

Expected: compiles with no errors (warnings about unused modules are fine).

- [ ] **Step 6: Commit**

```bash
cd ~/Tools/mission-control
git init
git add -A
git commit -m "feat: scaffold project with config, module stubs, and dependencies"
```

---

### Task 2: cmux Client (cmux/client.rs)

**Files:**
- Modify: `src/cmux/client.rs`

**Depends on:** Task 1 (Cargo.toml must exist for `cargo check`)

- [ ] **Step 1: Write cmux client**

`src/cmux/client.rs`:
```rust
use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub ref_id: String,      // e.g. "workspace:2"
    pub uuid: String,        // e.g. "32E47B1E-..."
    pub name: String,        // e.g. "gmail-labs"
    pub selected: bool,
}

pub struct CmuxClient {
    bin: String,
}

impl CmuxClient {
    pub fn new(bin: String) -> Self {
        Self { bin }
    }

    /// Parse `cmux list-workspaces --id-format both` output.
    /// Each line: `[*] workspace:N UUID  name [selected]`
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let output = Command::new(&self.bin)
            .args(["list-workspaces", "--id-format", "both"])
            .output()
            .await
            .context("failed to run cmux list-workspaces")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut workspaces = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let selected = line.starts_with('*');
            let line = line.trim_start_matches('*').trim();

            // Format: "workspace:N UUID  name  [selected]"
            let parts: Vec<&str> = line.splitn(3, char::is_whitespace).collect();
            if parts.len() < 3 {
                continue;
            }
            let ref_id = parts[0].to_string();
            let rest = parts[2..].join(" ").trim().to_string();

            // UUID is next token
            let mut rest_parts = rest.splitn(2, char::is_whitespace);
            let uuid = rest_parts.next().unwrap_or("").trim().to_string();
            let name = rest_parts
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches("[selected]")
                .trim()
                .to_string();

            workspaces.push(Workspace {
                ref_id,
                uuid,
                name,
                selected,
            });
        }

        Ok(workspaces)
    }

    /// Read the last N lines of a surface's screen.
    pub async fn read_screen(
        &self,
        workspace_ref: &str,
        lines: u32,
    ) -> Result<String> {
        let output = Command::new(&self.bin)
            .args([
                "read-screen",
                "--workspace",
                workspace_ref,
                "--lines",
                &lines.to_string(),
            ])
            .output()
            .await
            .context("failed to run cmux read-screen")?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Select a workspace (focus it).
    pub async fn select_workspace(&self, workspace_ref: &str) -> Result<()> {
        Command::new(&self.bin)
            .args(["select-workspace", "--workspace", workspace_ref])
            .output()
            .await
            .context("failed to run cmux select-workspace")?;
        Ok(())
    }
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

- [ ] **Step 3: Commit**

```bash
cd ~/Tools/mission-control
git add src/cmux/client.rs
git commit -m "feat: cmux client — list workspaces, read screen, select workspace"
```

---

### Task 3: cmux Event Stream (cmux/events.rs)

**Files:**
- Modify: `src/cmux/events.rs`

**Depends on:** Task 1

- [ ] **Step 1: Write the event stream parser**

`src/cmux/events.rs`:
```rust
use anyhow::Result;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct AgentEvent {
    pub session_id: String,
    pub workspace_id: String,
    pub tool_name: Option<String>,
    pub event_name: String,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    payload: Option<RawPayload>,
    #[serde(rename = "type")]
    event_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    phase: Option<String>,
}

/// Spawn `cmux events --reconnect --category agent --no-heartbeat` and stream parsed events.
pub async fn subscribe(
    cmux_bin: &str,
    tx: mpsc::UnboundedSender<AgentEvent>,
) -> Result<()> {
    let mut child = Command::new(cmux_bin)
        .args([
            "events",
            "--reconnect",
            "--category",
            "agent",
            "--no-heartbeat",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout from cmux events"))?;

    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let raw: RawEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Skip ack frames and non-event types
        if raw.event_type.as_deref() != Some("event") {
            continue;
        }

        let payload = match raw.payload {
            Some(p) => p,
            None => continue,
        };

        // Only process completed hook events (not received+completed duplicates)
        if payload.phase.as_deref() != Some("completed") {
            continue;
        }

        let session_id = match payload.session_id {
            Some(id) => id,
            None => continue,
        };

        let workspace_id = match raw.workspace_id {
            Some(id) => id,
            None => continue,
        };

        let event = AgentEvent {
            session_id,
            workspace_id,
            tool_name: payload.tool_name,
            event_name: raw.name.unwrap_or_default(),
        };

        if tx.send(event).is_err() {
            break; // receiver dropped
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

- [ ] **Step 3: Commit**

```bash
cd ~/Tools/mission-control
git add src/cmux/events.rs
git commit -m "feat: cmux event stream — subscribe to agent events via NDJSON"
```

---

### Task 4: Session File Parser (session/file.rs + session/watcher.rs)

**Files:**
- Modify: `src/session/file.rs`
- Modify: `src/session/watcher.rs`

**Depends on:** Task 1

- [ ] **Step 1: Write session file parser/serializer**

`src/session/file.rs`:
```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frontmatter {
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionFile {
    pub path: PathBuf,
    pub frontmatter: Frontmatter,
    pub bullets: Vec<String>,
    pub trajectory: Option<String>,
    pub next_steps: Vec<String>,
    /// Raw body text that isn't bullets/trajectory/next_steps
    pub other_body: String,
}

impl SessionFile {
    /// Parse a session markdown file with YAML frontmatter.
    pub fn parse(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse_str(&content, path.to_path_buf())
    }

    pub fn parse_str(content: &str, path: PathBuf) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(content)?;

        let mut bullets = Vec::new();
        let mut trajectory = None;
        let mut next_steps = Vec::new();
        let mut other_body = String::new();
        let mut section: Option<&str> = None;

        for line in body.lines() {
            if line.starts_with("## Trajectory") {
                section = Some("trajectory");
                continue;
            } else if line.starts_with("## Next Steps") {
                section = Some("next_steps");
                continue;
            } else if line.starts_with("## ") {
                section = Some("other");
            }

            match section {
                None => {
                    let trimmed = line.trim();
                    if trimmed.starts_with("- ") && !trimmed.starts_with("- [ ]") && !trimmed.starts_with("- [x]") {
                        bullets.push(trimmed.trim_start_matches("- ").to_string());
                    } else if !trimmed.is_empty() {
                        other_body.push_str(line);
                        other_body.push('\n');
                    }
                }
                Some("trajectory") => {
                    let trimmed = line.trim().trim_start_matches("> ").trim();
                    if !trimmed.is_empty() {
                        trajectory = Some(trimmed.to_string());
                    }
                }
                Some("next_steps") => {
                    let trimmed = line.trim();
                    if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") {
                        next_steps.push(trimmed.to_string());
                    }
                }
                _ => {
                    other_body.push_str(line);
                    other_body.push('\n');
                }
            }
        }

        Ok(SessionFile {
            path,
            frontmatter,
            bullets,
            trajectory,
            next_steps,
            other_body,
        })
    }

    /// Serialize back to markdown string.
    pub fn to_markdown(&self) -> Result<String> {
        let yaml = serde_yaml::to_string(&self.frontmatter)?;
        let mut out = format!("---\n{}---\n\n", yaml);

        for bullet in &self.bullets {
            out.push_str(&format!("- {}\n", bullet));
        }

        if let Some(ref traj) = self.trajectory {
            out.push_str(&format!("\n## Trajectory\n> {}\n", traj));
        }

        if !self.next_steps.is_empty() {
            out.push_str("\n## Next Steps\n");
            for step in &self.next_steps {
                out.push_str(&format!("{}\n", step));
            }
        }

        if !self.other_body.trim().is_empty() {
            out.push('\n');
            out.push_str(self.other_body.trim());
            out.push('\n');
        }

        Ok(out)
    }

    /// Write the session file back to disk.
    pub fn write(&self) -> Result<()> {
        let content = self.to_markdown()?;
        std::fs::write(&self.path, content)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

fn split_frontmatter(content: &str) -> Result<(Frontmatter, String)> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return Ok((Frontmatter::default(), content.to_string()));
    }

    let after_first = &content[3..];
    let end = after_first
        .find("\n---")
        .context("no closing --- in frontmatter")?;

    let yaml_str = &after_first[..end];
    let body = &after_first[end + 4..]; // skip \n---

    let frontmatter: Frontmatter = serde_yaml::from_str(yaml_str.trim())
        .unwrap_or_default();

    Ok((frontmatter, body.trim_start_matches('\n').to_string()))
}

/// Find all session files in a directory, sorted by modification time (newest first).
pub fn list_session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "md")
                && p.file_name()
                    .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
        })
        .collect();

    files.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma) // newest first
    });

    Ok(files)
}
```

- [ ] **Step 2: Write the session file watcher**

`src/session/watcher.rs`:
```rust
use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub struct SessionWatcher {
    _watcher: RecommendedWatcher,
}

#[derive(Debug, Clone)]
pub struct FileChanged {
    pub path: PathBuf,
}

impl SessionWatcher {
    /// Watch the histories directory for file changes.
    /// Sends FileChanged events through the channel.
    pub fn new(
        dir: PathBuf,
        tx: mpsc::UnboundedSender<FileChanged>,
    ) -> Result<Self> {
        let mut watcher = notify::recommended_watcher(
            move |res: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            for path in event.paths {
                                if path.extension().is_some_and(|e| e == "md") {
                                    let _ = tx.send(FileChanged { path });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            },
        )?;

        watcher.watch(&dir, RecursiveMode::NonRecursive)?;

        Ok(SessionWatcher { _watcher: watcher })
    }
}
```

- [ ] **Step 3: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

- [ ] **Step 4: Commit**

```bash
cd ~/Tools/mission-control
git add src/session/
git commit -m "feat: session file parser, serializer, and fsnotify watcher"
```

---

### Task 5: LLM Summarizer (llm/)

**Files:**
- Modify: `src/llm/mod.rs`
- Modify: `src/llm/openai.rs`

**Depends on:** Task 1

- [ ] **Step 1: Write the Summarizer trait and OpenAI implementation**

`src/llm/mod.rs`:
```rust
pub mod openai;

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
```

`src/llm/openai.rs`:
```rust
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

        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message {
                role: "user".to_string(),
                content: prompt,
            }],
            max_tokens: 512,
            temperature: 0.3,
        };

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

        let text = response
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        parse_summary(&text)
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
```

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

- [ ] **Step 3: Commit**

```bash
cd ~/Tools/mission-control
git add src/llm/
git commit -m "feat: LLM summarizer trait + OpenAI gpt-5.0 implementation"
```

---

### Task 6: App State (tui/app.rs)

**Files:**
- Modify: `src/tui/app.rs`

**Depends on:** Tasks 2, 3, 4, 5 (uses types from all modules)

- [ ] **Step 1: Write the App struct and update logic**

`src/tui/app.rs`:
```rust
use crate::cmux::client::{CmuxClient, Workspace};
use crate::cmux::events::AgentEvent;
use crate::config::Config;
use crate::llm::{Summary, Summarizer};
use crate::session::file::{self, SessionFile};
use crate::session::watcher::FileChanged;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WorkspaceState {
    pub workspace: Workspace,
    pub session: Option<SessionFile>,
    pub screen_preview: Option<String>,
    pub tool_call_count: u32,
    pub show_screen: bool,
}

pub struct App {
    pub workspaces: Vec<WorkspaceState>,
    pub selected: usize,
    pub should_quit: bool,
    /// Map session_id -> workspace UUID for fast lookup
    session_to_workspace: HashMap<String, String>,
    /// Map workspace UUID -> index in workspaces vec
    workspace_index: HashMap<String, usize>,
    /// Track bullet hashes to detect changes (for remote sessions)
    bullet_hashes: HashMap<PathBuf, u64>,
}

impl App {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            selected: 0,
            should_quit: false,
            session_to_workspace: HashMap::new(),
            workspace_index: HashMap::new(),
            bullet_hashes: HashMap::new(),
        }
    }

    /// Load workspaces from cmux and match session files.
    pub async fn refresh_workspaces(
        &mut self,
        client: &CmuxClient,
        histories_dir: &std::path::Path,
    ) -> Result<()> {
        let workspaces = client.list_workspaces().await?;
        let session_files = file::list_session_files(histories_dir).unwrap_or_default();

        // Parse all session files and index by workspace_id
        let mut sessions_by_workspace: HashMap<String, SessionFile> = HashMap::new();
        for path in &session_files {
            if let Ok(sf) = SessionFile::parse(path) {
                if let Some(ref ws_id) = sf.frontmatter.workspace_id {
                    // Keep the most recent file per workspace (list is sorted newest first)
                    sessions_by_workspace.entry(ws_id.clone()).or_insert(sf);
                }
            }
        }

        // Preserve tool_call_counts from existing state
        let old_counts: HashMap<String, u32> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.tool_call_count))
            .collect();

        self.workspaces = workspaces
            .into_iter()
            .map(|ws| {
                let session = sessions_by_workspace.remove(&ws.uuid);
                let tool_call_count = old_counts.get(&ws.uuid).copied().unwrap_or(0);
                WorkspaceState {
                    workspace: ws,
                    session,
                    screen_preview: None,
                    tool_call_count,
                    show_screen: false,
                }
            })
            .collect();

        // Sort: active agents first, then by name
        self.workspaces.sort_by(|a, b| {
            let a_active = a.session.as_ref().is_some_and(|s| {
                s.frontmatter.status.as_deref() == Some("active")
            });
            let b_active = b.session.as_ref().is_some_and(|s| {
                s.frontmatter.status.as_deref() == Some("active")
            });
            b_active
                .cmp(&a_active)
                .then_with(|| a.workspace.name.cmp(&b.workspace.name))
        });

        // Rebuild index
        self.workspace_index.clear();
        for (i, ws) in self.workspaces.iter().enumerate() {
            self.workspace_index.insert(ws.workspace.uuid.clone(), i);
        }

        Ok(())
    }

    /// Handle an incoming cmux agent event.
    pub fn handle_agent_event(&mut self, event: &AgentEvent) {
        // Register session -> workspace mapping
        self.session_to_workspace
            .insert(event.session_id.clone(), event.workspace_id.clone());

        // Increment tool call counter
        if let Some(&idx) = self.workspace_index.get(&event.workspace_id) {
            self.workspaces[idx].tool_call_count += 1;
        }
    }

    /// Check if a workspace needs LLM re-summarization based on tool call threshold.
    pub fn needs_summary(&self, workspace_uuid: &str, threshold: u32) -> bool {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            let ws = &self.workspaces[idx];
            ws.tool_call_count >= threshold && ws.session.is_some()
        } else {
            false
        }
    }

    /// Reset tool call counter after summarization.
    pub fn reset_tool_count(&mut self, workspace_uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            self.workspaces[idx].tool_call_count = 0;
        }
    }

    /// Update a workspace's session with new LLM summary.
    pub fn apply_summary(&mut self, workspace_uuid: &str, summary: Summary) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            if let Some(ref mut session) = self.workspaces[idx].session {
                session.trajectory = Some(summary.trajectory);
                session.next_steps = summary.next_steps;
            }
        }
    }

    /// Handle a file change notification — reload the session file.
    pub fn handle_file_changed(&mut self, changed: &FileChanged) -> Option<String> {
        // Try to parse the changed file
        let sf = SessionFile::parse(&changed.path).ok()?;
        let ws_id = sf.frontmatter.workspace_id.clone()?;

        // Check if bullets changed (for remote session LLM trigger)
        let new_hash = hash_bullets(&sf.bullets);
        let old_hash = self.bullet_hashes.get(&changed.path).copied();
        self.bullet_hashes.insert(changed.path.clone(), new_hash);
        let bullets_changed = old_hash.is_some_and(|h| h != new_hash);

        // Update the workspace's session
        if let Some(&idx) = self.workspace_index.get(&ws_id) {
            self.workspaces[idx].session = Some(sf);
        }

        if bullets_changed {
            Some(ws_id)
        } else {
            None
        }
    }

    pub fn selected_workspace(&self) -> Option<&WorkspaceState> {
        self.workspaces.get(self.selected)
    }

    pub fn next(&mut self) {
        if !self.workspaces.is_empty() {
            self.selected = (self.selected + 1) % self.workspaces.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.workspaces.is_empty() {
            self.selected = (self.selected + self.workspaces.len() - 1) % self.workspaces.len();
        }
    }
}

fn hash_bullets(bullets: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bullets.hash(&mut hasher);
    hasher.finish()
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

- [ ] **Step 3: Commit**

```bash
cd ~/Tools/mission-control
git add src/tui/app.rs
git commit -m "feat: app state with workspace management, event handling, and summary tracking"
```

---

### Task 7: TUI Widgets (tui/sidebar.rs + tui/detail.rs)

**Files:**
- Modify: `src/tui/sidebar.rs`
- Modify: `src/tui/detail.rs`

**Depends on:** Task 6

- [ ] **Step 1: Write the sidebar widget**

`src/tui/sidebar.rs`:
```rust
use crate::tui::app::WorkspaceState;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

pub fn render_sidebar(
    f: &mut Frame,
    area: Rect,
    workspaces: &[WorkspaceState],
    selected: usize,
) {
    let items: Vec<ListItem> = workspaces
        .iter()
        .map(|ws| {
            let (dot, dot_color) = status_indicator(ws);
            let host_badge = ws
                .session
                .as_ref()
                .and_then(|s| s.frontmatter.host.as_deref())
                .filter(|h| *h != "mbp") // don't badge local machine
                .map(|h| format!(" [{}]", h))
                .unwrap_or_default();

            let line = Line::from(vec![
                Span::styled(format!("{} ", dot), Style::default().fg(dot_color)),
                Span::styled(
                    ws.workspace.name.clone(),
                    Style::default().fg(Color::White),
                ),
                Span::styled(host_badge, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(" Workspaces ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .bg(Color::DarkGray),
        );

    let mut state = ListState::default();
    state.select(Some(selected));

    f.render_stateful_widget(list, area, &mut state);
}

fn status_indicator(ws: &WorkspaceState) -> (&str, Color) {
    match ws.session.as_ref().and_then(|s| s.frontmatter.status.as_deref()) {
        Some("active") => ("\u{25cf}", Color::Green),    // ●
        Some("idle") => ("\u{25d0}", Color::Yellow),     // ◐
        Some("waiting") => ("\u{26a0}", Color::Red),     // ⚠
        Some("done") => ("\u{25cb}", Color::DarkGray),   // ○
        _ => ("\u{25cb}", Color::DarkGray),              // ○ no agent
    }
}
```

- [ ] **Step 2: Write the detail pane widget**

`src/tui/detail.rs`:
```rust
use crate::tui::app::WorkspaceState;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

pub fn render_detail(f: &mut Frame, area: Rect, ws: Option<&WorkspaceState>) {
    let block = Block::default()
        .title(" Detail ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let ws = match ws {
        Some(ws) => ws,
        None => {
            f.render_widget(
                Paragraph::new("No workspace selected").block(block),
                area,
            );
            return;
        }
    };

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split detail into sections
    let has_session = ws.session.is_some();
    let has_screen = ws.show_screen && ws.screen_preview.is_some();

    let constraints = if has_session && has_screen {
        vec![
            Constraint::Length(3),  // header
            Constraint::Length(2),  // trajectory
            Constraint::Min(4),    // bullets + next steps
            Constraint::Length(12), // screen preview
        ]
    } else if has_session {
        vec![
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(4),
        ]
    } else if has_screen {
        vec![Constraint::Length(3), Constraint::Min(4)]
    } else {
        vec![Constraint::Length(3), Constraint::Min(1)]
    };

    let chunks = Layout::vertical(constraints).split(inner);
    let mut chunk_idx = 0;

    // Header
    render_header(f, chunks[chunk_idx], ws);
    chunk_idx += 1;

    if let Some(ref session) = ws.session {
        // Trajectory
        let traj_text = session
            .trajectory
            .as_deref()
            .unwrap_or("No trajectory yet");
        let traj = Paragraph::new(Line::from(vec![
            Span::styled("  ", Style::default().fg(Color::Cyan)),
            Span::raw(traj_text),
        ]));
        f.render_widget(traj, chunks[chunk_idx]);
        chunk_idx += 1;

        // Bullets + Next Steps
        let mut lines: Vec<Line> = Vec::new();

        if !session.bullets.is_empty() {
            lines.push(Line::from(Span::styled(
                "Progress:",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            for bullet in &session.bullets {
                lines.push(Line::from(format!("  - {}", bullet)));
            }
        }

        if !session.next_steps.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Next Steps:",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            for step in &session.next_steps {
                let color = if step.contains("[x]") {
                    Color::DarkGray
                } else {
                    Color::Yellow
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}", step),
                    Style::default().fg(color),
                )));
            }
        }

        let body = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        f.render_widget(body, chunks[chunk_idx]);
        chunk_idx += 1;
    }

    // Screen preview
    if has_screen {
        if let Some(ref preview) = ws.screen_preview {
            let screen = Paragraph::new(preview.as_str())
                .block(
                    Block::default()
                        .title(" Screen ")
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(screen, chunks[chunk_idx]);
        }
    } else if !has_session {
        let hint = Paragraph::new("No agent session. Press 's' for screen preview.");
        f.render_widget(hint, chunks[chunk_idx]);
    }
}

fn render_header(f: &mut Frame, area: Rect, ws: &WorkspaceState) {
    let status = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.status.as_deref())
        .unwrap_or("--");

    let agent = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.agent.as_deref())
        .unwrap_or("");

    let host = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.host.as_deref())
        .unwrap_or("");

    let topic = ws
        .session
        .as_ref()
        .and_then(|s| s.frontmatter.topic.as_deref())
        .unwrap_or("");

    let header_line = Line::from(vec![
        Span::styled(
            format!(" {} ", ws.workspace.name),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {} ", status), status_color(status)),
        Span::styled(format!(" {} ", agent), Style::default().fg(Color::Cyan)),
        Span::styled(format!(" {} ", host), Style::default().fg(Color::Magenta)),
    ]);

    let topic_line = if !topic.is_empty() {
        Line::from(Span::styled(
            format!("  topic: {}", topic),
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::raw("")
    };

    let header = Paragraph::new(vec![header_line, topic_line]);
    f.render_widget(header, area);
}

fn status_color(status: &str) -> Style {
    match status {
        "active" => Style::default().fg(Color::Black).bg(Color::Green),
        "idle" => Style::default().fg(Color::Black).bg(Color::Yellow),
        "waiting" => Style::default().fg(Color::White).bg(Color::Red),
        "done" => Style::default().fg(Color::White).bg(Color::DarkGray),
        _ => Style::default().fg(Color::DarkGray),
    }
}
```

- [ ] **Step 3: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

- [ ] **Step 4: Commit**

```bash
cd ~/Tools/mission-control
git add src/tui/sidebar.rs src/tui/detail.rs
git commit -m "feat: TUI widgets — sidebar with status dots, detail pane with trajectory/next-steps"
```

---

### Task 8: Main Event Loop (main.rs)

**Files:**
- Modify: `src/main.rs`

**Depends on:** Tasks 1-7

- [ ] **Step 1: Write the main event loop**

`src/main.rs`:
```rust
mod cmux;
mod config;
mod llm;
mod session;
mod tui;

use crate::cmux::client::CmuxClient;
use crate::cmux::events;
use crate::config::Config;
use crate::llm::openai::OpenAISummarizer;
use crate::llm::Summarizer;
use crate::session::watcher::SessionWatcher;
use crate::tui::app::App;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::{Constraint, Layout}, Terminal};
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();
    let cmux_client = CmuxClient::new(config.cmux_bin.clone());

    // Initialize summarizer if API key is available
    let summarizer: Option<Arc<dyn Summarizer>> = config.openai_api_key.as_ref().map(|key| {
        Arc::new(OpenAISummarizer::new(
            key.clone(),
            config.model.clone(),
            config::SUMMARIZE_PROMPT.to_string(),
        )) as Arc<dyn Summarizer>
    });

    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &config, &cmux_client, summarizer).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &Config,
    cmux_client: &CmuxClient,
    summarizer: Option<Arc<dyn Summarizer>>,
) -> Result<()> {
    let mut app = App::new();

    // Initial workspace load
    app.refresh_workspaces(cmux_client, &config.histories_dir).await?;

    // Channel for cmux agent events
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cmux_bin = config.cmux_bin.clone();
    tokio::spawn(async move {
        let _ = events::subscribe(&cmux_bin, event_tx).await;
    });

    // Channel for session file changes
    let (file_tx, mut file_rx) = mpsc::unbounded_channel();
    let _watcher = SessionWatcher::new(config.histories_dir.clone(), file_tx)?;

    // Channel for completed LLM summaries
    let (summary_tx, mut summary_rx) = mpsc::unbounded_channel::<(String, crate::llm::Summary)>();

    // Periodic workspace refresh
    let mut refresh_interval = interval(Duration::from_secs(30));

    loop {
        // Draw
        terminal.draw(|f| {
            let chunks = Layout::horizontal([
                Constraint::Length(32),
                Constraint::Min(40),
            ])
            .split(f.area());

            tui::sidebar::render_sidebar(f, chunks[0], &app.workspaces, app.selected);
            tui::detail::render_detail(f, chunks[1], app.selected_workspace());
        })?;

        // Handle events with timeout for async channels
        tokio::select! {
            // Terminal key events (poll with short timeout)
            _ = tokio::task::spawn_blocking(|| event::poll(std::time::Duration::from_millis(50))) => {
                if event::poll(std::time::Duration::from_millis(0))? {
                    if let Event::Key(key) = event::read()? {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                                app.should_quit = true;
                            }
                            (KeyCode::Char('j') | KeyCode::Down, _) => app.next(),
                            (KeyCode::Char('k') | KeyCode::Up, _) => app.previous(),
                            (KeyCode::Enter, _) => {
                                if let Some(ws) = app.selected_workspace() {
                                    let _ = cmux_client
                                        .select_workspace(&ws.workspace.ref_id)
                                        .await;
                                }
                            }
                            (KeyCode::Char('s'), _) => {
                                if let Some(ws) = app.workspaces.get_mut(app.selected) {
                                    ws.show_screen = !ws.show_screen;
                                    if ws.show_screen && ws.screen_preview.is_none() {
                                        ws.screen_preview = cmux_client
                                            .read_screen(&ws.workspace.ref_id, 10)
                                            .await
                                            .ok();
                                    }
                                }
                            }
                            (KeyCode::Char('r'), _) => {
                                // Force refresh: reload session + trigger LLM
                                if let Some(ws) = app.workspaces.get(app.selected) {
                                    if let Some(ref session) = ws.session {
                                        let uuid = ws.workspace.uuid.clone();
                                        let context = session.bullets.join("\n");
                                        if let Some(ref summarizer) = summarizer {
                                            let summarizer = Arc::clone(summarizer);
                                            let tx = summary_tx.clone();
                                            tokio::spawn(async move {
                                                if let Ok(summary) = summarizer.summarize(&context).await {
                                                    let _ = tx.send((uuid, summary));
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            // cmux agent events
            Some(agent_event) = event_rx.recv() => {
                let ws_uuid = agent_event.workspace_id.clone();
                app.handle_agent_event(&agent_event);

                // Check if threshold reached for LLM summarization
                if app.needs_summary(&ws_uuid, config.summary_threshold) {
                    app.reset_tool_count(&ws_uuid);
                    if let Some(ref summarizer) = summarizer {
                        // Build context from session bullets
                        if let Some(&idx) = app.workspace_index_for(&ws_uuid) {
                            if let Some(ref session) = app.workspaces[idx].session {
                                let context = session.bullets.join("\n");
                                let summarizer = Arc::clone(summarizer);
                                let tx = summary_tx.clone();
                                let uuid = ws_uuid.clone();
                                tokio::spawn(async move {
                                    if let Ok(summary) = summarizer.summarize(&context).await {
                                        let _ = tx.send((uuid, summary));
                                    }
                                });
                            }
                        }
                    }
                }
            }

            // Session file changes
            Some(changed) = file_rx.recv() => {
                if let Some(ws_uuid) = app.handle_file_changed(&changed) {
                    // Bullets changed on a remote session — trigger LLM
                    if let Some(ref summarizer) = summarizer {
                        if let Some(&idx) = app.workspace_index_for(&ws_uuid) {
                            if let Some(ref session) = app.workspaces[idx].session {
                                let context = session.bullets.join("\n");
                                let summarizer = Arc::clone(summarizer);
                                let tx = summary_tx.clone();
                                let uuid = ws_uuid.clone();
                                tokio::spawn(async move {
                                    if let Ok(summary) = summarizer.summarize(&context).await {
                                        let _ = tx.send((uuid, summary));
                                    }
                                });
                            }
                        }
                    }
                }
            }

            // Completed LLM summaries
            Some((uuid, summary)) = summary_rx.recv() => {
                app.apply_summary(&uuid, summary.clone());
                // Write back to session file
                if let Some(&idx) = app.workspace_index_for(&uuid) {
                    if let Some(ref session) = app.workspaces[idx].session {
                        let mut updated = session.clone();
                        updated.trajectory = Some(summary.trajectory);
                        updated.next_steps = summary.next_steps;
                        let _ = updated.write();
                    }
                }
            }

            // Periodic workspace list refresh
            _ = refresh_interval.tick() => {
                let _ = app.refresh_workspaces(cmux_client, &config.histories_dir).await;
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
```

Note: This requires adding a `workspace_index_for` method to `App`. Add to `src/tui/app.rs`:

```rust
    pub fn workspace_index_for(&self, uuid: &str) -> Option<&usize> {
        self.workspace_index.get(uuid)
    }
```

- [ ] **Step 2: Verify it compiles**

```bash
cd ~/Tools/mission-control && cargo check
```

Fix any type mismatches or import issues. The key potential issue is the `event::poll` inside `tokio::select!` — crossterm's poll is blocking. The pattern above uses a spawn_blocking wrapper with a short poll to keep the async loop responsive.

- [ ] **Step 3: Run it**

```bash
cd ~/Tools/mission-control && cargo run
```

Expected: TUI launches, shows workspace sidebar, detail pane. Pressing `q` exits cleanly.

- [ ] **Step 4: Commit**

```bash
cd ~/Tools/mission-control
git add src/main.rs src/tui/app.rs
git commit -m "feat: main event loop — terminal setup, cmux events, file watcher, LLM integration"
```

---

### Task 9: Mission Control Hook (agents configs repo)

**Files:**
- Create: `~/Projects/agents/configs/claude/hooks/mission-control-hook.sh`
- Create: `~/Projects/agents/configs/codex/hooks/mission-control-hook.sh` (symlink)
- Modify: `~/Projects/agents/configs/claude/settings.json` (add hook entries)
- Modify: `~/Projects/agents/configs/codex/hooks.json` (add hook entries)

**Depends on:** Nothing (fully independent)

- [ ] **Step 1: Write the hook script**

`~/Projects/agents/configs/claude/hooks/mission-control-hook.sh`:
```bash
#!/bin/bash
# mission-control-hook.sh — Stamp session history files with workspace/conversation metadata.
# Installed on SessionStart and Stop for Claude Code and Codex.
#
# Reads JSON from stdin (Claude Code hook protocol).
# Env: CMUX_WORKSPACE_ID (set by cmux terminals)

set -euo pipefail

HISTORIES_DIR="${HOME}/agents/histories"
DEVICE_FILE="${HOME}/agents/.device"
DEVICE="unknown"
[ -f "$DEVICE_FILE" ] && DEVICE="$(cat "$DEVICE_FILE" | tr -d '[:space:]')"

# Determine agent type
if [ "${CLAUDECODE:-}" = "1" ]; then
    AGENT="claude"
elif [ -n "${CODEX_SESSION_ID:-}" ]; then
    AGENT="codex"
else
    AGENT="unknown"
fi

# Read hook JSON from stdin
INPUT="$(cat)"
HOOK_EVENT="$(echo "$INPUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('hook_event_name',''))" 2>/dev/null || echo "")"
SESSION_ID="$(echo "$INPUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('session_id',''))" 2>/dev/null || echo "")"

# Extract conversation ID from session_id (format: "claude-<uuid>" or just "<uuid>")
CONV_ID="${SESSION_ID#claude-}"
CONV_ID="${CONV_ID#codex-}"

WORKSPACE_ID="${CMUX_WORKSPACE_ID:-}"

# Find the session file: match by today's date files, prefer ones already stamped with this conversation
TODAY="$(TZ=America/Los_Angeles date +%Y-%m-%d)"
HOUR="$(TZ=America/Los_Angeles date +%H)"

find_session_file() {
    # First: look for a file already stamped with this conversation_id
    if [ -n "$CONV_ID" ]; then
        local match
        match="$(grep -rl "conversation_id: $CONV_ID" "$HISTORIES_DIR"/*.md 2>/dev/null | head -1 || true)"
        if [ -n "$match" ]; then
            echo "$match"
            return
        fi
    fi

    # Second: look for today's files matching this workspace
    if [ -n "$WORKSPACE_ID" ]; then
        local match
        match="$(grep -rl "workspace_id: $WORKSPACE_ID" "$HISTORIES_DIR"/${TODAY}-*.md 2>/dev/null | head -1 || true)"
        if [ -n "$match" ]; then
            echo "$match"
            return
        fi
    fi

    # Third: look for the most recent file from today without a workspace_id
    local files
    files="$(ls -t "$HISTORIES_DIR"/${TODAY}-*.md 2>/dev/null | head -5)"
    for f in $files; do
        if ! grep -q "^workspace_id:" "$f" 2>/dev/null; then
            echo "$f"
            return
        fi
    done

    echo ""
}

stamp_frontmatter() {
    local file="$1"
    local key="$2"
    local value="$3"

    if [ -z "$value" ]; then
        return
    fi

    # Check if key already exists in frontmatter
    if grep -q "^${key}:" "$file" 2>/dev/null; then
        # Update existing key (only within frontmatter block)
        sed -i '' "s/^${key}:.*/${key}: ${value}/" "$file"
    else
        # Add after the last frontmatter field (before closing ---)
        sed -i '' "/^---$/,/^---$/ { /^---$/! { /^---$/! s/^---$/${key}: ${value}\n---/ ; } }" "$file" 2>/dev/null || true
        # Simpler fallback: insert before second ---
        python3 -c "
import sys
lines = open('$file').readlines()
second_fence = -1
count = 0
for i, l in enumerate(lines):
    if l.strip() == '---':
        count += 1
        if count == 2:
            second_fence = i
            break
if second_fence > 0:
    # Check key doesn't already exist
    if not any('$key:' in l for l in lines[:second_fence]):
        lines.insert(second_fence, '$key: $value\n')
        open('$file', 'w').writelines(lines)
" 2>/dev/null || true
    fi
}

SESSION_FILE="$(find_session_file)"

if [ -z "$SESSION_FILE" ]; then
    exit 0
fi

case "$HOOK_EVENT" in
    SessionStart)
        stamp_frontmatter "$SESSION_FILE" "workspace_id" "$WORKSPACE_ID"
        stamp_frontmatter "$SESSION_FILE" "conversation_id" "$CONV_ID"
        stamp_frontmatter "$SESSION_FILE" "agent" "$AGENT"
        stamp_frontmatter "$SESSION_FILE" "host" "$DEVICE"
        stamp_frontmatter "$SESSION_FILE" "status" "active"
        ;;
    Stop)
        stamp_frontmatter "$SESSION_FILE" "status" "done"
        ;;
esac

exit 0
```

- [ ] **Step 2: Make it executable and symlink for codex**

```bash
chmod +x ~/Projects/agents/configs/claude/hooks/mission-control-hook.sh
ln -sf ../claude/hooks/mission-control-hook.sh ~/Projects/agents/configs/codex/hooks/mission-control-hook.sh
```

- [ ] **Step 3: Register the hook in Claude Code settings.json**

Read `~/Projects/agents/configs/claude/settings.json` and add the hook to `SessionStart` and `Stop` events:

Add to the `SessionStart` hooks array:
```json
{
    "type": "command",
    "command": "$HOME/.claude/hooks/mission-control-hook.sh",
    "timeout": 5
}
```

Add a new `Stop` hooks entry (or append to existing):
```json
"Stop": [
    {
        "hooks": [
            {
                "type": "command",
                "command": "$HOME/.claude/hooks/mission-control-hook.sh",
                "timeout": 5
            }
        ]
    }
]
```

- [ ] **Step 4: Register the hook in Codex hooks.json**

Read `~/Projects/agents/configs/codex/hooks.json` and add to `Stop` (Codex uses Stop, not SessionStart):

```json
{
    "type": "command",
    "command": "$HOME/.codex/hooks/mission-control-hook.sh",
    "timeout": 5
}
```

- [ ] **Step 5: Commit in agents repo**

```bash
cd ~/Projects/agents
git add configs/claude/hooks/mission-control-hook.sh configs/codex/hooks/mission-control-hook.sh configs/claude/settings.json configs/codex/hooks.json
git commit -m "feat: mission-control hook — stamps session files with workspace/conversation metadata"
```

---

## Execution Order

For maximum parallelism with subagents:

**Wave 1 (all parallel):** Tasks 1, 9
**Wave 2 (all parallel, after T1):** Tasks 2, 3, 4, 5
**Wave 3 (after T2-T5):** Task 6
**Wave 4 (after T6):** Task 7
**Wave 5 (after T7):** Task 8

Tasks 2-5 can all run in parallel once the scaffold from T1 exists, since they write to independent files with no cross-imports.
