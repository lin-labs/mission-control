use crate::llm::Summarizer;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

pub struct SurfaceSummaryInputs {
    pub kind: String, // "shell"
    pub cwd: String,
    pub recent_commands: Vec<String>, // last 15 tab-separated log lines
}

pub async fn summarize(
    summarizer: &Arc<dyn Summarizer>,
    inputs: &SurfaceSummaryInputs,
) -> Result<String> {
    let prompt = build_prompt(inputs);
    // surface_summary is a short, stateless call — pass empty system and full prompt as user.
    let response = summarizer.regenerate_trajectory("", &prompt).await?;
    // Trim to one line, max 80 chars
    let summary = response
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("$ shell")
        .trim()
        .chars()
        .take(80)
        .collect::<String>();
    Ok(summary)
}

pub fn write_summary_file(surface_dir: &Path, sid: &str, summary: &str) -> Result<()> {
    let path = surface_dir.join(format!("{sid}.summary"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create surface dir {parent:?}"))?;
    }
    std::fs::write(&path, summary)
        .with_context(|| format!("write summary file {path:?}"))?;
    Ok(())
}

fn build_prompt(inputs: &SurfaceSummaryInputs) -> String {
    let mut prompt = String::new();

    prompt.push_str("Summarize what this terminal surface is doing in ONE line (<= 80 chars).\n");
    prompt.push_str("Form: \"$ <verb-phrase>\" using the most recent meaningful command, OR\n");
    prompt.push_str("      \"<noun-phrase>\" if commands describe ongoing exploration.\n\n");

    prompt.push_str(&format!("Surface kind: {}\n", inputs.kind));
    prompt.push_str(&format!("Working dir: {}\n", inputs.cwd));

    if !inputs.recent_commands.is_empty() {
        prompt.push_str("Last 15 commands (tab-separated: ts, rc, cwd, cmd):\n");
        for cmd in inputs.recent_commands.iter().rev().take(15).collect::<Vec<_>>().iter().rev() {
            prompt.push_str(cmd);
            prompt.push('\n');
        }
    }

    prompt
}
