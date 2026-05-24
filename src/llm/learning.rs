use crate::llm::Summarizer;
use anyhow::Result;
use std::sync::Arc;

pub struct LearningInputs {
    pub workspace_uuid: String,
    pub workspace_name: String,
    pub project: String,
    pub duration: String,               // e.g. "3h 42m"
    pub surfaces_summary: Vec<String>,  // ["claude", "codex", "shell"]
    pub final_trajectory: String,       // markdown
    pub history_snapshots: Vec<String>, // markdown each
    pub inputs: Vec<String>,            // contents of inputs/*.txt
    pub events_jsonl: String,           // full events log
    pub session_history_files: Vec<String>, // for each agent surface, the content
    pub shell_logs: Vec<String>,        // for each shell surface, the content
    pub surface_summaries: Vec<String>,
}

#[derive(Debug)]
pub struct LearningOutputs {
    pub full_record_md: String,
    pub candidates_only_md: Option<String>, // proposals file content (Phase 6 promotion)
}

pub async fn produce_learning(
    summarizer: &Arc<dyn Summarizer>,
    inputs: &LearningInputs,
) -> Result<LearningOutputs> {
    let (system_prompt, user_prompt) = build_split_prompt(inputs);
    let response = summarizer.regenerate_trajectory(&system_prompt, &user_prompt).await?;
    // For Phase 5 v1: assume the LLM returns the full 9-section record.
    // Split off the "Prompt-optimization candidates" section into a separate proposals file.
    let candidates = extract_candidates_section(&response);
    let proposals = candidates.map(|c| format_as_proposals_file(&c, &inputs.workspace_name));
    Ok(LearningOutputs {
        full_record_md: response,
        candidates_only_md: proposals,
    })
}

/// Build the cached-system + fresh-data prompt per the spec.
/// Split the prompt into stable (system) and fresh (user) parts for prompt caching.
/// The system part contains the instructions; the user part contains workspace-specific data.
pub fn build_split_prompt(inputs: &LearningInputs) -> (String, String) {
    let combined = build_prompt(inputs);
    const SEPARATOR: &str = "[USER MESSAGE]
";
    if let Some(pos) = combined.find(SEPARATOR) {
        let system = combined[..pos].trim_end().to_string();
        let user = combined[pos + SEPARATOR.len()..].to_string();
        (system, user)
    } else {
        // Fallback: treat entire prompt as user message with empty system.
        (String::new(), combined)
    }
}

pub fn build_prompt(inputs: &LearningInputs) -> String {
    let mut prompt = String::new();

    // ── System section (stable, cacheable) ──────────────────────────────────
    prompt.push_str("[SYSTEM - stable context, may be cached]\n");
    prompt.push_str(&format!(
        "You are producing the 盖官定论 (authoritative final record) for workspace '{}'.\n",
        inputs.workspace_name
    ));
    prompt.push_str("This is a self-sufficient record that captures everything worth knowing\n");
    prompt.push_str("about a completed workspace: what was done, what was learned, and what\n");
    prompt.push_str("prompt-level improvements should be made.\n\n");
    prompt.push_str("Produce exactly 9 sections in this order:\n");
    prompt.push_str("1. ## Goal arc — 3-7 bullets citing snapshot numbers (e.g. [snap-3])\n");
    prompt.push_str("2. ## Final trajectory — verbatim content of the final trajectory.md\n");
    prompt.push_str("3. ## Key turns — 5-15 bullets citing event IDs/snapshots (most significant decisions)\n");
    prompt.push_str("4. ## Surfaces — per-surface narrative paragraph (what each surface did)\n");
    prompt.push_str("5. ## Outputs — concrete artifacts: files, commits, tests, docs\n");
    prompt.push_str("6. ## Tooling & infra improvements — friction points and suggestions\n");
    prompt.push_str("7. ## Skill recommendations — new or improved skills that would help\n");
    prompt.push_str("8. ## User prompt improvements — how the user could prompt more effectively\n");
    prompt.push_str("9. ## Prompt-optimization candidates — PATTERN/EXPANSION pairs in this format:\n");
    prompt.push_str("   - [ ] PATTERN: \"<trigger text>\"\n");
    prompt.push_str("         EXPANSION: \"<full instruction>\"\n");
    prompt.push_str("         confidence: high|med|low\n");
    prompt.push_str("         evidence: <which events/turns support this>\n\n");
    prompt.push_str("Rules:\n");
    prompt.push_str("- Be specific and actionable, not vague.\n");
    prompt.push_str("- Every claim should cite evidence (event ID, snapshot number, or surface).\n");
    prompt.push_str("- Section 9 candidates should have PATTERN strings that are realistic\n");
    prompt.push_str("  trigger phrases a user would actually type.\n");
    prompt.push_str("- Output nothing outside the 9 sections.\n\n");

    // ── User message section (fresh per workspace) ───────────────────────────
    prompt.push_str("[USER MESSAGE]\n");
    prompt.push_str(&format!(
        "Workspace: {}  Project: {}  Duration: {}\n",
        inputs.workspace_name, inputs.project, inputs.duration
    ));
    if !inputs.surfaces_summary.is_empty() {
        let surfaces = inputs.surfaces_summary.join(", ");
        prompt.push_str(&format!("Surfaces used: {surfaces}\n"));
    }
    prompt.push('\n');

    // Final trajectory
    prompt.push_str("## Final trajectory.md\n```\n");
    prompt.push_str(&inputs.final_trajectory);
    prompt.push_str("\n```\n\n");

    // History snapshots
    if !inputs.history_snapshots.is_empty() {
        prompt.push_str("## Trajectory history snapshots (chronological)\n");
        for (i, snap) in inputs.history_snapshots.iter().enumerate() {
            prompt.push_str(&format!("### [snap-{}]\n```\n{}\n```\n\n", i + 1, snap));
        }
    }

    // User inputs
    if !inputs.inputs.is_empty() {
        prompt.push_str("## User inputs (inputs/N.txt)\n");
        for (i, inp) in inputs.inputs.iter().enumerate() {
            prompt.push_str(&format!("### [input-{}]\n{}\n\n", i + 1, inp.trim()));
        }
    }

    // Events log
    if !inputs.events_jsonl.trim().is_empty() {
        prompt.push_str("## Events log (events.jsonl)\n```\n");
        prompt.push_str(inputs.events_jsonl.trim());
        prompt.push_str("\n```\n\n");
    }

    // Agent session histories
    if !inputs.session_history_files.is_empty() {
        prompt.push_str("## Agent session histories\n");
        for (i, hist) in inputs.session_history_files.iter().enumerate() {
            let preview: String = hist.lines().take(100).collect::<Vec<_>>().join("\n");
            prompt.push_str(&format!("### Session {}\n{}\n\n", i + 1, preview.trim()));
        }
    }

    // Shell logs
    if !inputs.shell_logs.is_empty() {
        prompt.push_str("## Shell command logs\n");
        for (i, log) in inputs.shell_logs.iter().enumerate() {
            let preview: String = log.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            prompt.push_str(&format!("### Shell surface {}\n{}\n\n", i + 1, preview.trim()));
        }
    }

    // Surface summaries
    if !inputs.surface_summaries.is_empty() {
        prompt.push_str("## Surface summaries\n");
        for summary in &inputs.surface_summaries {
            prompt.push_str(&format!("- {}\n", summary.trim()));
        }
        prompt.push('\n');
    }

    prompt.push_str("Produce the full 9-section authoritative record now.\n");
    prompt
}

/// Find the `## Prompt-optimization candidates` heading in the response and
/// return everything below it until the next `## ` heading or EOF.
pub fn extract_candidates_section(response: &str) -> Option<String> {
    let heading = "## Prompt-optimization candidates";
    let start_pos = response.find(heading)?;
    // Start after the heading line (skip to next newline)
    let after_heading = &response[start_pos..];
    let content_start = after_heading.find('\n').map(|p| p + 1).unwrap_or(after_heading.len());
    let content = &after_heading[content_start..];

    // Find the next `## ` heading after this one, or take the rest.
    let end = find_next_section_offset(content);
    let section_content = content[..end].trim().to_string();

    if section_content.is_empty() {
        None
    } else {
        Some(section_content)
    }
}

/// Return the byte offset of the next `## ` heading in `s`, or `s.len()` if none.
fn find_next_section_offset(s: &str) -> usize {
    let mut offset = 0usize;
    for line in s.lines() {
        if offset > 0 && line.starts_with("## ") {
            return offset;
        }
        offset += line.len() + 1; // +1 for '\n'
    }
    s.len()
}

/// Wrap candidate bullets in the Phase 6 proposals header.
pub fn format_as_proposals_file(candidates: &str, workspace_name: &str) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut out = String::new();
    out.push_str(&format!(
        "# Prompt-optimization candidates — {workspace_name} {date}\n\n"
    ));
    out.push_str("Tick the rules you want to promote to `rules.md`.\n");
    out.push_str(
        "Run `mc promote-rules <this-file>` to apply checked rules.\n\n"
    );
    out.push_str("Rules:\n\n");
    out.push_str(candidates);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_candidates_returns_none_when_absent() {
        let resp = "## Goal arc\n- did stuff\n\n## Final trajectory\n(traj)\n";
        assert!(extract_candidates_section(resp).is_none());
    }

    #[test]
    fn extract_candidates_returns_section_content() {
        let resp = "## Goal arc\n- did stuff\n\n## Prompt-optimization candidates\n- [ ] PATTERN: \"foo\"\n      EXPANSION: \"bar\"\n";
        let result = extract_candidates_section(resp);
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("PATTERN:"), "should contain PATTERN line");
        assert!(content.contains("foo"));
    }

    #[test]
    fn extract_candidates_stops_at_next_heading() {
        let resp = "## Prompt-optimization candidates\n- [ ] PATTERN: \"foo\"\n\n## Other section\n- not this\n";
        let content = extract_candidates_section(resp).unwrap();
        assert!(!content.contains("not this"), "should not include content past next heading");
        assert!(content.contains("foo"));
    }

    #[test]
    fn format_as_proposals_file_has_header_and_candidates() {
        let candidates = "- [ ] PATTERN: \"build X\"\n      EXPANSION: \"Use Y approach\"\n      confidence: high\n";
        let result = format_as_proposals_file(candidates, "my-workspace");
        assert!(result.contains("my-workspace"));
        assert!(result.contains("Tick the rules you want to promote"));
        assert!(result.contains("promote-rules"));
        assert!(result.contains("PATTERN:"));
    }

    #[test]
    fn build_prompt_contains_trajectory() {
        let inputs = LearningInputs {
            workspace_uuid: "uuid-1".to_string(),
            workspace_name: "test-ws".to_string(),
            project: "test-project".to_string(),
            duration: "1h 30m".to_string(),
            surfaces_summary: vec!["claude".to_string()],
            final_trajectory: "## Goal\n- Build a thing\n".to_string(),
            history_snapshots: vec![],
            inputs: vec![],
            events_jsonl: String::new(),
            session_history_files: vec![],
            shell_logs: vec![],
            surface_summaries: vec![],
        };
        let prompt = build_prompt(&inputs);
        assert!(prompt.contains("Build a thing"), "prompt should contain trajectory content");
        assert!(prompt.contains("test-ws"), "prompt should contain workspace name");
    }
}
