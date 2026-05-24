use super::log::{CallTimer, log_call};
use super::{Summarizer, Summary};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

/// Summarizer backed by the locally-installed `codex exec` CLI.
/// Uses the user's existing Codex authentication — no API key needed.
pub struct CodexSummarizer {
    bin: String,
    prompt_template: String,
    model: Option<String>,
}

impl CodexSummarizer {
    pub fn new(bin: String, prompt_template: String, model: Option<String>) -> Self {
        Self {
            bin,
            prompt_template,
            model,
        }
    }

    fn build_prompt(&self, context: &str) -> String {
        self.prompt_template.replace("{context}", context)
    }
}

#[async_trait]
impl Summarizer for CodexSummarizer {
    async fn summarize(&self, context: &str) -> Result<Summary> {
        let prompt = self.build_prompt(context);
        let timer = CallTimer::start();
        let result = self.summarize_inner(&prompt).await;
        match &result {
            Ok(text) => log_call("codex", &prompt, Ok(text.as_str()), timer.ms()),
            Err(e) => log_call("codex", &prompt, Err(&format!("{:#}", e)), timer.ms()),
        }
        let text = result?;
        parse_summary(&text)
    }

    async fn regenerate_trajectory(&self, system: &str, user: &str) -> Result<String> {
        // Codex CLI doesn't support prompt caching — just concatenate the parts.
        let prompt = format!("{system}\n\n{user}");
        let timer = CallTimer::start();
        let result = self.summarize_inner(&prompt).await;
        match &result {
            Ok(text) => log_call("codex-regen", &prompt, Ok(text.as_str()), timer.ms()),
            Err(e) => log_call("codex-regen", &prompt, Err(&format!("{:#}", e)), timer.ms()),
        }
        result
    }
}

impl CodexSummarizer {
    async fn summarize_inner(&self, prompt: &str) -> Result<String> {
        let tmp = tempfile_path()?;

        let mut cmd = Command::new(&self.bin);
        cmd.arg("exec")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("--color")
            .arg("never")
            .arg("--output-last-message")
            .arg(&tmp)
            .arg("-");
        if let Some(ref m) = self.model {
            cmd.arg("-m").arg(m);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {} exec", self.bin))?;

        // Write the prompt to stdin and close it
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("failed to write prompt to codex stdin")?;
            stdin.shutdown().await.ok();
        }

        // 90s timeout — codex exec for a tiny summary should be well under this
        let output = match timeout(Duration::from_secs(90), child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => anyhow::bail!("codex wait failed: {}", e),
            Err(_) => anyhow::bail!("codex exec timed out after 90s"),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr_short: String = stderr.lines().take(5).collect::<Vec<_>>().join(" | ");
            anyhow::bail!(
                "codex exec failed ({}): {}",
                output.status,
                stderr_short.trim()
            );
        }

        // Prefer the dedicated last-message file; fall back to stdout
        let text = match tokio::fs::read_to_string(&tmp).await {
            Ok(s) if !s.trim().is_empty() => s,
            _ => String::from_utf8_lossy(&output.stdout).to_string(),
        };
        let _ = tokio::fs::remove_file(&tmp).await;

        if text.trim().is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr_short: String = stderr.lines().take(5).collect::<Vec<_>>().join(" | ");
            anyhow::bail!(
                "codex returned empty output. stderr: {}",
                stderr_short.trim()
            );
        }

        Ok(text)
    }
}

fn tempfile_path() -> Result<std::path::PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(std::env::temp_dir().join(format!("mc-codex-summary-{}.txt", ts)))
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
        // Fallback: use first non-empty line as trajectory
        trajectory = text
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("Summary unavailable")
            .to_string();
    }

    Ok(Summary {
        trajectory,
        next_steps,
    })
}
