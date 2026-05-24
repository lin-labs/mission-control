use crate::llm::Summarizer;
use crate::mc_data::events::Event;
use crate::mc_data::trajectory::TrajectoryDoc;
use anyhow::{Context, Result};
use std::sync::Arc;

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
    let doc = TrajectoryDoc::parse(&response)
        .with_context(|| "LLM returned invalid markdown for trajectory regeneration")?;
    Ok(doc)
}

pub fn build_prompt(inputs: &RegenInputs) -> RegenPrompt {
    // ── System section (stable, cacheable) ──────────────────────────────────
    let mut system = String::new();
    system.push_str(&format!(
        "You maintain a 3-section trajectory doc for workspace '{}'.\n",
        inputs.workspace_name
    ));
    system.push_str("Sections (exact order): ## Mission, ## Current surfaces, ## Goals & Progress.\n\n");
    system.push_str("Rules:\n");
    system.push_str("- User edits are TYPED ACTIONS. Interpret intent:\n");
    system.push_str("  check -> user marked done; don't re-open the item\n");
    system.push_str("  delete -> user judged not meaningful; don't re-add similar\n");
    system.push_str("  add -> user identified gap; preserve and build on it\n");
    system.push_str("  edit -> user rephrased; treat new phrasing as authoritative\n");
    system.push_str("  move -> user re-ordered; respect the priority signal\n");
    system.push_str("- Mission section is continuously refined, never replaced wholesale.\n");
    system.push_str("- Each `## Current surfaces` line ends with `<!-- mc:surface:<sid> -->`.\n");
    system.push_str("  Preserve these markers exactly. Do not invent surface IDs.\n");
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
        "- Tool calls executed: {}\n",
        inputs.tool_call_count
    ));

    if !inputs.cmux_surface_order.is_empty() {
        let order = inputs.cmux_surface_order.join(", ");
        user.push_str(&format!("Cmux surface order: [{order}]\n"));
    }

    user.push_str("\nProduce the new trajectory.md.\n");

    RegenPrompt { system, user }
}
