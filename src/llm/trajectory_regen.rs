use crate::llm::Summarizer;
use crate::mc_data::events::Event;
use crate::mc_data::trajectory::{Item, SECTION_MISSION, TrajectoryDoc};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

/// Hard cap on Mission bullets. The prompt asks the model to stay under this;
/// `clean_mission` enforces it deterministically (and strips process-noise
/// bullets) so a misbehaving model can't grow Mission unbounded.
pub const MISSION_MAX_BULLETS: usize = 6;
const MISSION_FALLBACK_MAX_BULLETS: usize = 3;
const MISSION_BULLET_MAX_CHARS: usize = 110;

pub struct RegenInputs {
    pub workspace_name: String,
    pub current_trajectory: String, // markdown
    pub recent_events: Vec<Event>,
    pub recent_user_explanations: Vec<String>,
    pub session_bullets: Vec<String>,
    pub surface_summaries: Vec<(String, String)>, // (surface_id, one-liner)
    pub tool_call_count: u32,
    pub cmux_surface_order: Vec<String>, // ordered surface IDs
    /// Canonical user ask from ~obsAgents/Sessions/.../<file>.md (last `## boyan` block).
    /// Takes precedence over any screen-scraped user_prompt.
    pub user_ask: Option<String>,
}

/// Split prompt: stable system context (cacheable) + fresh user-turn data.
pub struct RegenPrompt {
    /// Stable across calls — instructions + workspace identity.  Tagged for
    /// OpenAI prompt-caching (cache_control: ephemeral on the system message).
    pub system: String,
    /// Fresh per call — recent events, user explanations, signals.
    pub user: String,
}

pub async fn regenerate(
    summarizer: &Arc<dyn Summarizer>,
    inputs: &RegenInputs,
) -> Result<TrajectoryDoc> {
    let prompt = build_prompt(inputs);
    let response = summarizer
        .regenerate_trajectory(&prompt.system, &prompt.user)
        .await?;
    let mut doc = TrajectoryDoc::parse(&response)
        .with_context(|| "LLM returned invalid markdown for trajectory regeneration")?;
    doc.ensure_sections();
    reconcile_mission(&mut doc, inputs);
    Ok(doc)
}

/// Make Mission deterministic around the LLM:
///
/// 1. Saved `[h]` Mission rows remain byte-for-byte authoritative.
/// 2. Model rows are accepted only when they do not substantially overlap a
///    human row; model-authored `[h]` markers are stripped.
/// 3. Completed Mission history is always copied from the saved trajectory.
/// 4. An empty result is repaired from direct human input, compact conversation
///    summaries, and finally the workspace name.
fn reconcile_mission(doc: &mut TrajectoryDoc, inputs: &RegenInputs) {
    clean_mission(doc);

    let mut saved = TrajectoryDoc::parse(&inputs.current_trajectory).unwrap_or_default();
    saved.ensure_sections();
    let completed_items = saved.mission_history.clone();
    doc.mission_history = completed_items.clone();

    let human_items: Vec<Item> = saved
        .section(SECTION_MISSION)
        .into_iter()
        .flat_map(|section| section.items.iter())
        .filter(|item| is_human_mission(&item.text) && !item.text.trim().is_empty())
        .cloned()
        .collect();
    let mut merged = human_items.clone();
    let generated_items = doc
        .section(SECTION_MISSION)
        .map(|section| section.items.clone())
        .unwrap_or_default();
    for mut item in generated_items {
        let generated_text = strip_human_marker(&item.text);
        if generated_text.is_empty()
            || human_items
                .iter()
                .any(|human| missions_are_similar(&human.text, &generated_text))
            || completed_items
                .iter()
                .any(|completed| missions_are_similar(&completed.text, &generated_text))
            || merged
                .iter()
                .filter(|existing| !is_human_mission(&existing.text))
                .any(|existing| missions_are_similar(&existing.text, &generated_text))
        {
            continue;
        }
        item.text = generated_text;
        item.is_checkbox = true;
        item.checked = Some(false);
        item.surface_id = None;
        merged.push(item);
    }

    if merged.is_empty() && completed_items.is_empty() {
        merged = fallback_mission_items(inputs);
    }
    for item in &mut merged {
        item.is_checkbox = true;
        item.checked = Some(false);
        item.surface_id = None;
    }
    doc.replace_section_items(SECTION_MISSION, merged);
}

fn fallback_mission_items(inputs: &RegenInputs) -> Vec<Item> {
    if let Some(ask) = inputs.user_ask.as_deref().and_then(short_mission_bullet) {
        return vec![mission_item(ask)];
    }

    if let Some(explanation) = inputs
        .recent_user_explanations
        .iter()
        .rev()
        .find_map(|text| short_mission_bullet(text))
    {
        return vec![mission_item(explanation)];
    }

    let mut bullets = Vec::new();
    let mut seen = HashSet::new();
    let summaries = inputs.session_bullets.iter().map(String::as_str).chain(
        inputs
            .surface_summaries
            .iter()
            .map(|(_, summary)| summary.as_str()),
    );
    for summary in summaries {
        let Some(bullet) = short_mission_bullet(summary) else {
            continue;
        };
        if seen.insert(bullet.to_lowercase()) {
            bullets.push(mission_item(bullet));
        }
        if bullets.len() == MISSION_FALLBACK_MAX_BULLETS {
            break;
        }
    }

    if bullets.is_empty() {
        let fallback = short_mission_bullet(&format!("Continue work in {}", inputs.workspace_name))
            .unwrap_or_else(|| "Clarify the workspace mission".to_string());
        bullets.push(mission_item(fallback));
    }
    bullets
}

fn mission_item(text: String) -> Item {
    Item {
        text,
        is_checkbox: true,
        checked: Some(false),
        surface_id: None,
    }
}

fn is_human_mission(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed == "[h]"
        || trimmed == "[H]"
        || trimmed.starts_with("[h] ")
        || trimmed.starts_with("[H] ")
}

fn strip_human_marker(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("[h]")
        .or_else(|| trimmed.strip_prefix("[H]"))
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

fn mission_tokens(text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "for", "from", "in", "of", "on", "or", "the", "this", "to", "with",
    ];
    let normalized = strip_human_marker(text)
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let mut tokens = normalized
        .split_whitespace()
        .filter(|token| !STOP_WORDS.contains(token))
        .map(str::to_string)
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn missions_are_similar(left: &str, right: &str) -> bool {
    let left = mission_tokens(left);
    let right = mission_tokens(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }
    let shared = left.iter().filter(|token| right.contains(token)).count();
    let smaller = left.len().min(right.len());
    (smaller >= 2 && shared == smaller) || (shared >= 3 && shared * 4 >= smaller * 3)
}

fn short_mission_bullet(text: &str) -> Option<String> {
    let mut text = text.trim();
    if let Some(stripped) = text.strip_prefix("- [ ] ") {
        text = stripped;
    } else if let Some(stripped) = text.strip_prefix("- [x] ") {
        text = stripped;
    } else if let Some(stripped) = text.strip_prefix("- [X] ") {
        text = stripped;
    } else if let Some(stripped) = text.strip_prefix("- ") {
        text = stripped;
    }
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    if normalized.chars().count() <= MISSION_BULLET_MAX_CHARS {
        return Some(normalized);
    }
    let mut shortened = normalized
        .chars()
        .take(MISSION_BULLET_MAX_CHARS - 1)
        .collect::<String>();
    shortened.push('…');
    Some(shortened)
}

/// Strip process-noise bullets from the Mission section and cap it at
/// `MISSION_MAX_BULLETS`. Runs on every regen so existing bloated docs are
/// pruned the next time they regenerate, not just freshly-written ones.
pub fn clean_mission(doc: &mut TrajectoryDoc) {
    use crate::mc_data::trajectory::SECTION_MISSION;
    let Some(mission) = doc.sections.iter_mut().find(|s| s.name == SECTION_MISSION) else {
        return;
    };
    mission
        .items
        .retain(|item| is_human_mission(&item.text) || !is_mission_noise(&item.text));
    let agent_count = mission
        .items
        .iter()
        .filter(|item| !is_human_mission(&item.text))
        .count();
    let mut agents_to_drop = agent_count.saturating_sub(MISSION_MAX_BULLETS);
    mission.items.retain(|item| {
        if agents_to_drop > 0 && !is_human_mission(&item.text) {
            agents_to_drop -= 1;
            false
        } else {
            true
        }
    });
    for item in &mut mission.items {
        item.is_checkbox = true;
        item.checked = Some(false);
        item.surface_id = None;
    }
}

/// True for Mission bullets that merely narrate activity/process rather than
/// stating a durable goal or constraint — the unbounded filler the old prompt
/// produced (e.g. "Latest 111-tool-call signal adds process only…").
fn is_mission_noise(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("adds process only")
        || t.contains("adds process not scope")
        || t.contains("tool-call signal")
        || t.contains("tool call signal")
        || (t.contains("tool-call") && t.contains("signal"))
        || t.contains("tool calls executed")
}

pub fn build_prompt(inputs: &RegenInputs) -> RegenPrompt {
    // ── System section (stable, cacheable) ──────────────────────────────────
    let mut system = String::new();
    system.push_str(&format!(
        "You maintain a 4-section trajectory doc for workspace '{}'.\n",
        inputs.workspace_name
    ));
    system.push_str(
        "Sections (exact order): ## Mission, ## Mission history, ## Current surfaces, ## Beads.\n\n",
    );
    system.push_str("Rules:\n");
    system.push_str("- User edits are TYPED ACTIONS. Interpret intent:\n");
    system.push_str("  check -> user marked done; don't re-open the item\n");
    system.push_str("  delete -> user judged not meaningful; don't re-add similar\n");
    system.push_str("  add -> user identified gap; preserve and build on it\n");
    system.push_str("  edit -> user rephrased; treat new phrasing as authoritative\n");
    system.push_str("  move -> user re-ordered; respect the priority signal\n");
    system.push_str("- `## Mission` may be empty only when `## Mission history` contains completed work. Otherwise emit active Mission rows. Human-authored Mission text is primary. Preserve all `[h]` Mission rows byte-for-byte and keep them first.\n");
    system.push_str("  Do not add `[h]` to agent-authored rows, edit a human row, or emit an agent row similar to a human row.\n");
    system.push_str("- Copy `## Mission history` verbatim. Never revive, rewrite, or remove its completed rows.\n");
    system.push_str("  When no saved Mission exists, write one very short bullet (under ~110 characters) from the user's latest ask.\n");
    system.push_str("  If there is no latest ask, write up to three very short bullets summarizing the active conversations and surface focus.\n");
    system.push_str("  Do not append instructions, old asks, process rules, subtask checklists, activity/tool-call counts, or \"adds process only\" / \"adds process not scope\" filler.\n");
    system.push_str("  If prior work is still useful context, represent it as Beads done/open rows when grounded; otherwise leave it out.\n");
    system.push_str("- Each `## Current surfaces` line ends with `<!-- mc:surface:<sid> -->`.\n");
    system.push_str("  Preserve these markers exactly. Do not invent surface IDs.\n");
    system.push_str("- `## Beads` is sourced from repo-local Beads issues when available.\n");
    system.push_str("  Do not invent Beads issue IDs; preserve existing Beads rows if unsure.\n");
    system.push_str("- Output the full new trajectory.md verbatim — no commentary.\n");

    // ── User message section (fresh per call) ────────────────────────────────
    let mut user = String::new();

    // Include canonical user ask when available (takes precedence over screen-scraped prompt).
    if let Some(ref ask) = inputs.user_ask {
        let trimmed = ask.trim();
        if !trimmed.is_empty() {
            let truncated = if trimmed.len() > 500 {
                &trimmed[..500]
            } else {
                trimmed
            };
            user.push_str("The user's latest ask (from session log):\n");
            user.push_str(truncated);
            user.push_str("\n\n");
        }
    }

    user.push_str("Last saved trajectory.md:\n");
    user.push_str("```\n");
    user.push_str(&inputs.current_trajectory);
    user.push_str("\n```\n\n");

    // Recent events (last 20)
    if !inputs.recent_events.is_empty() {
        user.push_str("Recent user actions (events.jsonl tail, last 20):\n");
        for event in inputs
            .recent_events
            .iter()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            if let Ok(json) = serde_json::to_string(event) {
                user.push_str(&json);
                user.push('\n');
            }
        }
        user.push('\n');
    }

    // User explanations (last 3)
    if !inputs.recent_user_explanations.is_empty() {
        user.push_str("User explanations (inputs/N.txt tails, last 3):\n");
        for explanation in inputs
            .recent_user_explanations
            .iter()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .iter()
            .rev()
        {
            user.push_str("---\n");
            user.push_str(explanation);
            user.push('\n');
        }
        user.push_str("---\n\n");
    }

    // New signals
    user.push_str("New signals since last regen:\n");
    if !inputs.session_bullets.is_empty() {
        user.push_str("- Session bullets per agent surface:\n");
        for bullet in &inputs.session_bullets {
            user.push_str(&format!("  {bullet}\n"));
        }
    }
    if !inputs.surface_summaries.is_empty() {
        user.push_str("- Surface focus summaries:\n");
        for (sid, summary) in &inputs.surface_summaries {
            user.push_str(&format!("  [{sid}] {summary}\n"));
        }
    }
    user.push_str(&format!(
        "- Activity since last regen: {} tool calls (context only — do NOT add a Mission \
         bullet about this count).\n",
        inputs.tool_call_count
    ));

    if !inputs.cmux_surface_order.is_empty() {
        let order = inputs.cmux_surface_order.join(", ");
        user.push_str(&format!("Cmux surface order: [{order}]\n"));
    }

    user.push_str("\nProduce the new trajectory.md.\n");

    RegenPrompt { system, user }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mc_data::trajectory::{SECTION_MISSION, TrajectoryDoc};

    fn mission_bullets(doc: &TrajectoryDoc) -> Vec<String> {
        doc.sections
            .iter()
            .find(|s| s.name == SECTION_MISSION)
            .map(|s| s.items.iter().map(|i| i.text.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn clean_mission_strips_process_noise() {
        let md = "---\n{}\n---\n\n## Mission\n\
            - Ship the remote-surface intent feature\n\
            - Latest 111-tool-call signal adds process only, not a change in scope attribution.\n\
            - Validate work directly before reporting status\n\
            - Latest 50-tool-call signal adds process only.\n\
            \n## Current surfaces\n\n## Beads\n";
        let mut doc = TrajectoryDoc::parse(md).unwrap();
        clean_mission(&mut doc);
        let bullets = mission_bullets(&doc);
        assert_eq!(
            bullets.len(),
            2,
            "noise bullets should be gone: {bullets:?}"
        );
        assert!(
            bullets
                .iter()
                .all(|b| !b.to_lowercase().contains("adds process only"))
        );
    }

    #[test]
    fn clean_mission_caps_bullet_count() {
        let mut md = String::from("---\n{}\n---\n\n## Mission\n");
        for i in 0..12 {
            md.push_str(&format!("- durable goal number {i}\n"));
        }
        md.push_str("\n## Current surfaces\n\n## Beads\n");
        let mut doc = TrajectoryDoc::parse(&md).unwrap();
        clean_mission(&mut doc);
        let bullets = mission_bullets(&doc);
        assert_eq!(bullets.len(), MISSION_MAX_BULLETS);
        // Keeps the most recent (tail) bullets.
        assert_eq!(bullets.last().unwrap(), "durable goal number 11");
    }

    #[test]
    fn is_mission_noise_matches_filler_only() {
        assert!(is_mission_noise(
            "Latest 63-tool-call signal adds process only"
        ));
        assert!(is_mission_noise("Tool calls executed: 88"));
        assert!(!is_mission_noise("Ship the remote-surface intent feature"));
    }
}
