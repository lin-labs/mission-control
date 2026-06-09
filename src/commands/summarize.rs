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
#[derive(Debug, Clone, Default)]
pub struct WorkspaceDigest {
    pub name: String,
    pub status_label: String,
    pub turn_count: usize,
    pub last_summary: Option<String>,
    pub next_steps: Vec<String>,
    /// Total surfaces in this cmux workspace (terminals + browsers).
    pub num_surfaces: usize,
    /// Surfaces currently driving an agent (Claude / Codex / etc.).
    pub num_agent_surfaces: usize,
    /// Rough char count of the session's bullets — proxy for context volume.
    /// We surface a token estimate (chars / 4) in the report.
    pub session_chars: usize,
    /// Workspace's git working dir, if cmux reports one. Used by the async
    /// `gather_commit_stats` pass to count recent commits.
    pub cwd: Option<PathBuf>,
    /// Filled in by `gather_commit_stats`; left as `None` when no git repo
    /// is at `cwd` or the lookup fails.
    pub commits_24h: Option<CommitStats>,
    /// Per-workspace cmux description (if set). Cmux's own one-line workspace
    /// summary.
    pub description: Option<String>,
    /// Mission section text from trajectory.md (one bullet per line, joined
    /// with `\n`). Empty when the file doesn't exist or has no Mission.
    pub mission: String,
    /// Beads items, partitioned by checkbox state. Sourced from
    /// trajectory.md so they survive process restarts (unlike `last_summary`
    /// which only exists in the running TUI's in-memory state).
    pub open_goals: Vec<String>,
    pub done_goals: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CommitStats {
    pub count: usize,
    /// One-line subjects of the most recent commits (capped at 5).
    pub recent: Vec<String>,
}

/// Build digests for every visible workspace.
/// Reads only from `&App` — no I/O — so it is safe to call on the UI thread.
pub fn collect_digests(app: &App) -> Vec<WorkspaceDigest> {
    use crate::mc_data::trajectory::{SECTION_GOALS, SECTION_MISSION};

    app.workspaces
        .iter()
        .map(|ws| {
            let num_surfaces = ws.surfaces.len();
            let num_agent_surfaces = ws.surfaces.iter().filter(|s| s.kind.is_agent()).count();
            let session_chars = ws
                .session
                .as_ref()
                .map(|s| s.bullets.iter().map(|b| b.len()).sum())
                .unwrap_or(0);
            let cwd = ws
                .workspace
                .current_directory
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);

            // Pull Mission + Beads from trajectory.md — these are persisted
            // to disk so a fresh `mc summarize` invocation still sees them
            // (unlike `ws.summary` which only exists in TUI memory).
            let (mission, open_goals, done_goals) = ws
                .trajectory
                .as_ref()
                .map(|doc| {
                    let mission_lines: Vec<String> = doc
                        .section(SECTION_MISSION)
                        .map(|s| {
                            s.items
                                .iter()
                                .map(|i| i.text.trim().to_string())
                                .filter(|t| !t.is_empty())
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut open: Vec<String> = Vec::new();
                    let mut done: Vec<String> = Vec::new();
                    if let Some(goals_sec) = doc.section(SECTION_GOALS) {
                        for item in &goals_sec.items {
                            let txt = item.text.trim().to_string();
                            if txt.is_empty() {
                                continue;
                            }
                            match item.checked {
                                Some(true) => done.push(txt),
                                _ => open.push(txt),
                            }
                        }
                    }
                    (mission_lines.join("\n"), open, done)
                })
                .unwrap_or_default();

            WorkspaceDigest {
                name: ws.workspace.name.clone(),
                status_label: derive_status_label(ws),
                turn_count: ws.session.as_ref().map(|s| s.bullets.len()).unwrap_or(0),
                last_summary: ws.summary.as_ref().map(|s| s.trajectory.clone()),
                next_steps: ws
                    .summary
                    .as_ref()
                    .map(|s| s.next_steps.clone())
                    .unwrap_or_default(),
                num_surfaces,
                num_agent_surfaces,
                session_chars,
                cwd,
                commits_24h: None,
                description: ws.workspace.description.clone(),
                mission,
                open_goals,
                done_goals,
            }
        })
        .collect()
}

/// Aggregate stats across all workspaces. Pure computation from digests.
#[derive(Debug, Clone, Default)]
pub struct SummaryStats {
    pub workspaces: usize,
    pub surfaces: usize,
    pub agent_surfaces: usize,
    pub turns: usize,
    /// chars / 4 — rough ChatGPT-style token estimate of the aggregated
    /// session-log context.
    pub token_estimate: usize,
    pub commits_24h: usize,
}

impl SummaryStats {
    pub fn from_digests(digests: &[WorkspaceDigest]) -> Self {
        let mut s = Self::default();
        s.workspaces = digests.len();
        for d in digests {
            s.surfaces += d.num_surfaces;
            s.agent_surfaces += d.num_agent_surfaces;
            s.turns += d.turn_count;
            s.token_estimate += d.session_chars / 4;
            if let Some(c) = d.commits_24h.as_ref() {
                s.commits_24h += c.count;
            }
        }
        s
    }
}

/// Run `git log --since="24 hours ago"` for each workspace's cwd, in
/// parallel via `tokio::spawn`. Skips workspaces with no cwd or whose cwd
/// isn't a git repo. Fills in `WorkspaceDigest::commits_24h` in place.
pub async fn gather_commit_stats(digests: &mut [WorkspaceDigest]) {
    let mut handles = Vec::with_capacity(digests.len());
    for (idx, d) in digests.iter().enumerate() {
        if let Some(cwd) = d.cwd.clone() {
            handles.push(tokio::spawn(
                async move { (idx, commit_stats_for(&cwd).await) },
            ));
        }
    }
    for h in handles {
        if let Ok((idx, Some(stats))) = h.await {
            if let Some(d) = digests.get_mut(idx) {
                d.commits_24h = Some(stats);
            }
        }
    }
}

async fn commit_stats_for(cwd: &Path) -> Option<CommitStats> {
    use tokio::process::Command;
    // Use `git -C <cwd> log` rather than `cd` so we don't have to worry
    // about working dir state. `--no-pager` keeps cmd-line clean.
    let output = Command::new("git")
        .args([
            "-C",
            cwd.to_str().unwrap_or("."),
            "--no-pager",
            "log",
            "--since=24 hours ago",
            "--oneline",
            "--no-color",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let recent: Vec<String> = stdout.lines().map(|l| l.to_string()).take(5).collect();
    let count = stdout.lines().count();
    Some(CommitStats { count, recent })
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
/// Under the Obsidian Agents vault, reached via $OBS_AGENTS or the stable
/// ~/agents/obsAgents symlink (-> obs/Agents) — never a hardcoded iCloud path,
/// and never the nonexistent ~/agents/Obsidian path.
pub fn output_dir() -> PathBuf {
    if let Ok(v) = std::env::var("OBS_AGENTS") {
        return PathBuf::from(v).join("mc-workspaces-summaries");
    }
    let home = dirs::home_dir().unwrap_or_default();
    home.join("agents/obsAgents/mc-workspaces-summaries")
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
    let stats = SummaryStats::from_digests(digests);
    let mut s = String::new();
    s.push_str("# Snapshot data\n\n");
    s.push_str(&format!(
        "**Totals** — {} workspaces · {} surfaces ({} agent) · {} session turns · ~{} tokens of context · {} commits in last 24h\n\n",
        stats.workspaces,
        stats.surfaces,
        stats.agent_surfaces,
        stats.turns,
        stats.token_estimate,
        stats.commits_24h,
    ));
    for (i, d) in digests.iter().enumerate() {
        s.push_str(&format!(
            "## workspace {} — {} ({})\n",
            i + 1,
            d.name,
            d.status_label
        ));
        s.push_str(&format!(
            "surfaces: {} ({} agent) · turns: {} · ~{} tokens\n",
            d.num_surfaces,
            d.num_agent_surfaces,
            d.turn_count,
            d.session_chars / 4,
        ));
        if let Some(ref cwd) = d.cwd {
            s.push_str(&format!("cwd: {}\n", cwd.display()));
        }
        if let Some(ref desc) = d.description {
            if !desc.trim().is_empty() {
                s.push_str(&format!("cmux description: {}\n", desc.trim()));
            }
        }
        if let Some(ref c) = d.commits_24h {
            s.push_str(&format!("commits (24h): {}\n", c.count));
            for line in &c.recent {
                s.push_str(&format!("  · {}\n", line));
            }
        }
        s.push('\n');
        if !d.mission.trim().is_empty() {
            s.push_str("Mission (from trajectory.md):\n");
            s.push_str(d.mission.trim());
            s.push_str("\n\n");
        }
        if !d.done_goals.is_empty() {
            s.push_str("Beads done:\n");
            for g in &d.done_goals {
                s.push_str(&format!("- [x] {}\n", g));
            }
            s.push('\n');
        }
        if !d.open_goals.is_empty() {
            s.push_str("Beads open:\n");
            for g in &d.open_goals {
                s.push_str(&format!("- [ ] {}\n", g));
            }
            s.push('\n');
        }
        if let Some(ref t) = d.last_summary {
            s.push_str("Most recent per-workspace summary (in-memory only — may be empty when running from CLI):\n");
            s.push_str(t.trim());
            s.push_str("\n\n");
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

pub const SUMMARIZE_INSTRUCTIONS: &str = r###"You are producing a daily-snapshot report for Boyan across his cmux
workspaces. The user message contains:
  - aggregate totals (workspaces, surfaces, turns, ~tokens, commits)
  - per-workspace digests (status, surface counts, session size, recent
    commits, prior summary, suggested next steps)

NOTE: the writer (mc) will prepend a "## Statistics" section computed
from the same data — DO NOT repeat the raw totals. Start with section 1
below and assume the reader already saw the numbers.

Produce a Markdown report with exactly these four sections:

## 1. Per-workspace impact
For each workspace with meaningful activity, one block:
### <name>  ·  <status>  ·  <N> turns  ·  <K> commits
- Done: 1–3 concrete things shipped or made progress on
- Impact: 1 sentence on why it matters / what's now possible
- Ideas worth following up: 0–2 specific ideas the work surfaced
Skip workspaces with no activity. Group similar workspaces if the
report would otherwise be too repetitive.

## 2. Human-side improvements
What patterns suggest Boyan could work better. Tone: candid coaching, not
flattery. Examples of the right shape:
- "You restarted X three times before checking Y — establish that
  diagnostic step first next time."
- "Five workspaces had unfinished edits at end of day; consider
  closing one before opening the next."
- "When work spans 2+ days, write the goal back in trajectory.md so the
  next session resumes faster."

## 3. Tooling / system-side improvements
What the tools (mc, cmux, agents, the project itself) could do better,
based on patterns visible in the snapshot:
- "The peek code path keeps showing the same conversation for
  same-workspace surfaces — the index resolver still has a gap."
- "Refresh tick blocks the UI when X happens."
- "Notes file would benefit from auto-archiving items >30 days old."
Be specific. Cite the workspace or behavior that revealed the gap.

## 4. Open questions
Things you noticed but couldn't conclude. 0–3 bullets.

Tone: terse, factual, no marketing language. Output Markdown only.
Do NOT emit YAML frontmatter (the writer adds it). Do NOT emit a
"## Statistics" section (the writer adds it).
"###;

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

/// Build the final document = YAML frontmatter + Statistics section + LLM body.
///
/// The Statistics section is computed deterministically from the digests
/// (totals + per-workspace numeric snapshot) so the report always has solid
/// numbers even when the LLM is unavailable or hallucinating. The qualitative
/// sections (per-workspace impact, human/system improvements) come from the
/// LLM body.
pub fn build_document(
    generated_at: DateTime<Local>,
    digests: &[WorkspaceDigest],
    body: &str,
) -> String {
    let stats = SummaryStats::from_digests(digests);
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("generated_at: {}\n", generated_at.to_rfc3339()));
    s.push_str(&format!("mc_version: {}\n", env!("CARGO_PKG_VERSION")));
    s.push_str(&format!("workspace_count: {}\n", stats.workspaces));
    s.push_str(&format!("surface_count: {}\n", stats.surfaces));
    s.push_str(&format!("agent_surfaces: {}\n", stats.agent_surfaces));
    s.push_str(&format!("turn_total: {}\n", stats.turns));
    s.push_str(&format!("token_estimate: {}\n", stats.token_estimate));
    s.push_str(&format!("commits_24h: {}\n", stats.commits_24h));
    if digests.is_empty() {
        s.push_str("workspaces: []\n");
    } else {
        s.push_str("workspaces:\n");
        for d in digests {
            s.push_str(&format!("  - name: {}\n", yaml_inline(&d.name)));
            s.push_str(&format!("    status: {}\n", d.status_label));
            s.push_str(&format!("    surfaces: {}\n", d.num_surfaces));
            s.push_str(&format!("    agent_surfaces: {}\n", d.num_agent_surfaces));
            s.push_str(&format!("    turns: {}\n", d.turn_count));
            s.push_str(&format!("    tokens: {}\n", d.session_chars / 4));
            s.push_str(&format!("    open_beads: {}\n", d.open_goals.len()));
            s.push_str(&format!("    done_beads: {}\n", d.done_goals.len()));
            if let Some(ref c) = d.commits_24h {
                s.push_str(&format!("    commits_24h: {}\n", c.count));
            }
        }
    }
    s.push_str("---\n\n");

    // ── Statistics (deterministic, never LLM-generated) ──
    s.push_str("# Snapshot\n\n");
    s.push_str("## Statistics\n\n");
    s.push_str(&format!("- **Workspaces**: {}\n", stats.workspaces));
    s.push_str(&format!(
        "- **Surfaces**: {} ({} running an agent)\n",
        stats.surfaces, stats.agent_surfaces
    ));
    s.push_str(&format!("- **Session turns**: {}\n", stats.turns));
    s.push_str(&format!(
        "- **Context volume**: ~{} tokens (chars ÷ 4)\n",
        stats.token_estimate
    ));
    s.push_str(&format!(
        "- **Commits in last 24h**: {}\n",
        stats.commits_24h
    ));

    let goals_done: usize = digests.iter().map(|d| d.done_goals.len()).sum();
    let goals_open: usize = digests.iter().map(|d| d.open_goals.len()).sum();
    s.push_str(&format!(
        "- **Beads**: {} done · {} open\n\n",
        goals_done, goals_open
    ));

    // Per-workspace one-line stat table
    s.push_str("### Per-workspace numbers\n\n");
    s.push_str("| workspace | status | surfaces (agent) | turns | tokens | beads done/open | commits 24h |\n");
    s.push_str("|---|---|---|---|---|---|---|\n");
    for d in digests {
        let commits = d.commits_24h.as_ref().map(|c| c.count).unwrap_or(0);
        s.push_str(&format!(
            "| {} | {} | {} ({}) | {} | {} | {}/{} | {} |\n",
            d.name,
            d.status_label,
            d.num_surfaces,
            d.num_agent_surfaces,
            d.turn_count,
            d.session_chars / 4,
            d.done_goals.len(),
            d.open_goals.len(),
            commits,
        ));
    }
    s.push('\n');

    // ── Qualitative body from LLM ──
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
