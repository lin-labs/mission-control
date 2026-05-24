//! `:summarize` command — produces a markdown snapshot of all visible
//! workspaces and writes it to the Obsidian vault.

use crate::commands::CommandResult;
use crate::llm::Summarizer;
use crate::tui::app::App;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Per-workspace snapshot taken on the UI thread before the LLM call.
#[derive(Debug, Clone)]
pub struct WorkspaceDigest {
    pub name: String,
    pub status_label: String,
    pub turn_count: usize,
    pub last_summary: Option<String>,
    pub next_steps: Vec<String>,
}

/// Build digests for every visible workspace.
/// Reads only from `&App` — no I/O — so it is safe to call on the UI thread.
pub fn collect_digests(app: &App) -> Vec<WorkspaceDigest> {
    app.workspaces
        .iter()
        .map(|ws| WorkspaceDigest {
            name: ws.workspace.name.clone(),
            status_label: derive_status_label(ws),
            turn_count: ws.session.as_ref().map(|s| s.bullets.len()).unwrap_or(0),
            last_summary: ws.summary.as_ref().map(|s| s.trajectory.clone()),
            next_steps: ws
                .summary
                .as_ref()
                .map(|s| s.next_steps.clone())
                .unwrap_or_default(),
        })
        .collect()
}

fn derive_status_label(ws: &crate::tui::app::WorkspaceState) -> String {
    // Reuse the same labels the sidebar shows.
    use crate::tui::app::AgentState;
    let state = if ws.summarizing {
        AgentState::Working
    } else if ws.screen_insights.activity.is_some() {
        AgentState::Working
    } else if ws.session.is_some() {
        AgentState::NeedsMe
    } else {
        AgentState::Idle
    };
    state.label().to_string()
}

/// Resolve the directory where mc writes summary reports.
/// Always under the iCloud-synced Obsidian Agents vault.
pub fn output_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    home.join(
        "Library/Mobile Documents/iCloud~md~obsidian/Documents/Agents/mc-workspaces-summaries",
    )
}

/// Compute the report path for `now`, falling back to minute-/second-suffixed
/// names if earlier candidates already exist on disk.
///
/// `dir` is taken explicitly so tests can pass a tempdir.
pub fn resolve_report_path(dir: &Path, now: DateTime<Local>) -> PathBuf {
    let base = now.format("%Y-%m-%d-%H-summary.md").to_string();
    let candidate = dir.join(&base);
    if !candidate.exists() {
        return candidate;
    }
    let with_min = dir.join(now.format("%Y-%m-%d-%H-%M-summary.md").to_string());
    if !with_min.exists() {
        return with_min;
    }
    dir.join(now.format("%Y-%m-%d-%H-%M-%S-summary.md").to_string())
}

/// Build the LLM prompt body (the user message). The system message is
/// constant and lives in `SUMMARIZE_INSTRUCTIONS`.
pub fn build_user_prompt(digests: &[WorkspaceDigest]) -> String {
    if digests.is_empty() {
        return String::from("(no workspaces visible)");
    }
    let mut s = String::new();
    s.push_str(&format!("{} workspace(s):\n\n", digests.len()));
    for (i, d) in digests.iter().enumerate() {
        s.push_str(&format!(
            "## workspace {} — {} ({})\n",
            i + 1,
            d.name,
            d.status_label
        ));
        s.push_str(&format!("turns: {}\n\n", d.turn_count));
        if let Some(ref t) = d.last_summary {
            s.push_str("Most recent per-workspace summary:\n");
            s.push_str(t.trim());
            s.push_str("\n\n");
        } else {
            s.push_str("(no per-workspace summary available)\n\n");
        }
        if !d.next_steps.is_empty() {
            s.push_str("Suggested next steps from per-workspace summary:\n");
            for ns in &d.next_steps {
                s.push_str(&format!("- {}\n", ns));
            }
            s.push('\n');
        }
    }
    s
}

pub const SUMMARIZE_INSTRUCTIONS: &str = r#"You are summarizing a snapshot of work-in-progress across N cmux workspaces.
Produce a human-readable Markdown report for Boyan with these sections:

## Overview
2–3 sentences. What got done across all workspaces, mood/momentum.

## Per-workspace
For each workspace, one block:
### <name>  ·  <status>  ·  <turns> turns
- Done: <bullets, concrete>
- To improve: <bullets, what's off / blocked / needs cleanup>

## What to do next
Cross-workspace todos, ranked. Be specific; cite workspace names.

Tone: terse, factual, no marketing language. Skip workspaces with no activity.
Output Markdown only. Do NOT emit YAML frontmatter (the writer adds it).
"#;

/// Strip a single leading YAML frontmatter block if the LLM emitted one.
pub fn strip_leading_frontmatter(body: &str) -> &str {
    let trimmed = body.trim_start_matches('\n');
    if !trimmed.starts_with("---\n") {
        return body;
    }
    // Find the closing `---\n`.
    let after_open = &trimmed[4..];
    if let Some(idx) = after_open.find("\n---\n") {
        let after_close = &after_open[idx + 5..];
        return after_close;
    }
    body
}

/// Build the final document = YAML frontmatter + body.
pub fn build_document(
    generated_at: DateTime<Local>,
    digests: &[WorkspaceDigest],
    body: &str,
) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("generated_at: {}\n", generated_at.to_rfc3339()));
    s.push_str(&format!("mc_version: {}\n", env!("CARGO_PKG_VERSION")));
    if digests.is_empty() {
        s.push_str("workspaces: []\n");
    } else {
        s.push_str("workspaces:\n");
        for d in digests {
            s.push_str(&format!("  - name: {}\n", yaml_inline(&d.name)));
            s.push_str(&format!("    status: {}\n", d.status_label));
            s.push_str(&format!("    turns: {}\n", d.turn_count));
        }
    }
    s.push_str("---\n\n");
    s.push_str(strip_leading_frontmatter(body).trim_start());
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Quote a string for safe inline YAML scalar use.
fn yaml_inline(v: &str) -> String {
    if v.is_empty()
        || v.contains(':')
        || v.contains('#')
        || v.contains('\n')
        || v.starts_with(' ')
        || v.ends_with(' ')
    {
        let escaped = v.replace('"', "\\\"");
        format!("\"{}\"", escaped)
    } else {
        v.to_string()
    }
}

/// Atomically write `contents` to `path`: write to `<path>.tmp`, then rename.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Run `:summarize` end-to-end. Called from a `tokio::spawn`d task.
pub async fn run(
    digests: Vec<WorkspaceDigest>,
    summarizer: Option<Arc<dyn Summarizer>>,
) -> CommandResult {
    let now = Local::now();
    let dir = output_dir();
    let path = resolve_report_path(&dir, now);

    // Empty case — skip LLM, write a stub.
    if digests.is_empty() {
        let doc = build_document(now, &digests, "(no active workspaces)\n");
        return match atomic_write(&path, &doc) {
            Ok(()) => CommandResult::SummarizeDone(path),
            Err(e) => CommandResult::Err(format!("write failed: {:#}", e)),
        };
    }

    // LLM available?
    let Some(summarizer) = summarizer else {
        return CommandResult::Err("no LLM configured".into());
    };

    let user = build_user_prompt(&digests);
    let body = match summarizer
        .regenerate_trajectory(SUMMARIZE_INSTRUCTIONS, &user)
        .await
    {
        Ok(b) => b,
        Err(e) => return CommandResult::Err(format!("LLM error: {:#}", e)),
    };

    let doc = build_document(now, &digests, &body);
    match atomic_write(&path, &doc) {
        Ok(()) => CommandResult::SummarizeDone(path),
        Err(e) => CommandResult::Err(format!("write failed: {:#}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Summarizer, Summary};
    use async_trait::async_trait;
    use chrono::TimeZone;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn fixed_time() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 5, 24, 14, 37, 2).unwrap()
    }

    #[test]
    fn resolve_path_uses_hour_when_no_collision() {
        let d = tempdir().unwrap();
        let p = resolve_report_path(d.path(), fixed_time());
        assert_eq!(p.file_name().unwrap(), "2026-05-24-14-summary.md");
    }

    #[test]
    fn resolve_path_falls_back_to_minute_on_collision() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("2026-05-24-14-summary.md"), "x").unwrap();
        let p = resolve_report_path(d.path(), fixed_time());
        assert_eq!(p.file_name().unwrap(), "2026-05-24-14-37-summary.md");
    }

    #[test]
    fn resolve_path_falls_back_to_second_on_double_collision() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("2026-05-24-14-summary.md"), "x").unwrap();
        std::fs::write(d.path().join("2026-05-24-14-37-summary.md"), "x").unwrap();
        let p = resolve_report_path(d.path(), fixed_time());
        assert_eq!(p.file_name().unwrap(), "2026-05-24-14-37-02-summary.md");
    }

    #[test]
    fn build_document_contains_frontmatter_and_body() {
        let digests = vec![WorkspaceDigest {
            name: "foo".to_string(),
            status_label: "running".to_string(),
            turn_count: 12,
            last_summary: None,
            next_steps: vec![],
        }];
        let doc = build_document(fixed_time(), &digests, "## Overview\nhi\n");
        assert!(doc.starts_with("---\n"));
        assert!(doc.contains("generated_at: 2026-05-24T14:37:02"));
        assert!(doc.contains("- name: foo"));
        assert!(doc.contains("    status: running"));
        assert!(doc.contains("    turns: 12"));
        assert!(doc.contains("## Overview\nhi"));
    }

    #[test]
    fn build_document_strips_leading_frontmatter_from_body() {
        let digests: Vec<WorkspaceDigest> = vec![];
        let body = "---\ngenerated_at: bogus\n---\n## Overview\nreal\n";
        let doc = build_document(fixed_time(), &digests, body);
        // The "bogus" frontmatter line should be gone; only ours remains.
        let bogus_count = doc.matches("bogus").count();
        assert_eq!(bogus_count, 0);
        assert!(doc.contains("## Overview\nreal"));
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let d = tempdir().unwrap();
        let nested = d.path().join("a/b/c/foo.md");
        atomic_write(&nested, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "hello");
    }

    #[test]
    fn build_user_prompt_empty() {
        assert_eq!(build_user_prompt(&[]), "(no workspaces visible)");
    }

    #[test]
    fn build_user_prompt_includes_workspace_name_and_turns() {
        let digests = vec![WorkspaceDigest {
            name: "alpha".to_string(),
            status_label: "working".to_string(),
            turn_count: 5,
            last_summary: Some("did stuff".to_string()),
            next_steps: vec!["next thing".to_string()],
        }];
        let prompt = build_user_prompt(&digests);
        assert!(prompt.contains("alpha"));
        assert!(prompt.contains("working"));
        assert!(prompt.contains("turns: 5"));
        assert!(prompt.contains("did stuff"));
        assert!(prompt.contains("- next thing"));
    }

    #[test]
    fn yaml_inline_quotes_when_needed() {
        assert_eq!(yaml_inline("foo"), "foo");
        assert_eq!(yaml_inline("foo: bar"), "\"foo: bar\"");
        assert_eq!(yaml_inline("a#b"), "\"a#b\"");
    }

    struct StubSummarizer {
        body: String,
    }

    #[async_trait]
    impl Summarizer for StubSummarizer {
        async fn summarize(&self, _ctx: &str) -> anyhow::Result<Summary> {
            Ok(Summary {
                trajectory: self.body.clone(),
                next_steps: vec![],
            })
        }

        async fn regenerate_trajectory(&self, _sys: &str, _user: &str) -> anyhow::Result<String> {
            Ok(self.body.clone())
        }
    }

    #[tokio::test]
    async fn end_to_end_pipeline_with_stub_summarizer() {
        // Exercises the full Summarizer→prompt→document→atomic_write chain.
        // We hand-roll the chain (rather than calling `run`) because `run` writes
        // to the real iCloud output_dir(); the test redirects to a tempdir.
        let dir = tempdir().unwrap();
        let now = chrono::Local::now();
        let path = resolve_report_path(dir.path(), now);

        let digests = vec![WorkspaceDigest {
            name: "alpha".to_string(),
            status_label: "working".to_string(),
            turn_count: 7,
            last_summary: Some("did A and B".to_string()),
            next_steps: vec!["do C".to_string()],
        }];

        let stub: Arc<dyn Summarizer> = Arc::new(StubSummarizer {
            body: "## Overview\nshipped.\n".to_string(),
        });
        let user_prompt = build_user_prompt(&digests);
        let body = stub
            .regenerate_trajectory(SUMMARIZE_INSTRUCTIONS, &user_prompt)
            .await
            .unwrap();
        let doc = build_document(now, &digests, &body);
        atomic_write(&path, &doc).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("---\n"));
        assert!(written.contains("- name: alpha"));
        assert!(written.contains("    turns: 7"));
        assert!(written.contains("## Overview\nshipped."));

        // Frontmatter-strip on the final doc should yield body alone.
        let stripped = strip_leading_frontmatter(&written);
        assert!(!stripped.contains("generated_at"));
    }
}
