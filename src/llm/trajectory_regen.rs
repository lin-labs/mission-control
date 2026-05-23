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
}

pub async fn regenerate(
    summarizer: &Arc<dyn Summarizer>,
    inputs: &RegenInputs,
) -> Result<TrajectoryDoc> {
    let prompt = build_prompt(inputs);
    let response = summarizer.regenerate_trajectory(&prompt).await?;
    let doc = TrajectoryDoc::parse(&response)
        .with_context(|| "LLM returned invalid markdown for trajectory regeneration")?;
    Ok(doc)
}

pub fn build_prompt(inputs: &RegenInputs) -> String {
    let mut prompt = String::new();

    // System / cached section
    prompt.push_str("[SYSTEM - stable context, may be cached]\n");
    prompt.push_str(&format!(
        "You maintain a 3-section trajectory doc for workspace '{}'.\n",
        inputs.workspace_name
    ));
    prompt.push_str("Sections (exact order): ## Goal, ## Current surfaces, ## Tasks & Progress.\n\n");
    prompt.push_str("Rules:\n");
    prompt.push_str("- User edits are TYPED ACTIONS. Interpret intent:\n");
    prompt.push_str("  check -> user marked done; don't re-open the item\n");
    prompt.push_str("  delete -> user judged not meaningful; don't re-add similar\n");
    prompt.push_str("  add -> user identified gap; preserve and build on it\n");
    prompt.push_str("  edit -> user rephrased; treat new phrasing as authoritative\n");
    prompt.push_str("  move -> user re-ordered; respect the priority signal\n");
    prompt.push_str("- Goal section is continuously refined, never replaced wholesale.\n");
    prompt.push_str("- Each `## Current surfaces` line ends with `<!-- mc:surface:<sid> -->`.\n");
    prompt.push_str("  Preserve these markers exactly. Do not invent surface IDs.\n");
    prompt.push_str("- Output the full new trajectory.md verbatim — no commentary.\n\n");

    // User message section
    prompt.push_str("[USER MESSAGE]\n");
    prompt.push_str("Last saved trajectory.md:\n");
    prompt.push_str("```\n");
    prompt.push_str(&inputs.current_trajectory);
    prompt.push_str("\n```\n\n");

    // Recent events (last 20)
    if !inputs.recent_events.is_empty() {
        prompt.push_str("Recent user actions (events.jsonl tail, last 20):\n");
        for event in inputs.recent_events.iter().rev().take(20).collect::<Vec<_>>().iter().rev() {
            if let Ok(json) = serde_json::to_string(event) {
                prompt.push_str(&json);
                prompt.push('\n');
            }
        }
        prompt.push('\n');
    }

    // User explanations (last 3)
    if !inputs.recent_user_explanations.is_empty() {
        prompt.push_str("User explanations (inputs/N.txt tails, last 3):\n");
        for explanation in inputs.recent_user_explanations.iter().rev().take(3).collect::<Vec<_>>().iter().rev() {
            prompt.push_str("---\n");
            prompt.push_str(explanation);
            prompt.push('\n');
        }
        prompt.push_str("---\n\n");
    }

    // New signals
    prompt.push_str("New signals since last regen:\n");
    if !inputs.session_bullets.is_empty() {
        prompt.push_str("- Session bullets per agent surface:\n");
        for bullet in &inputs.session_bullets {
            prompt.push_str(&format!("  {bullet}\n"));
        }
    }
    if !inputs.surface_summaries.is_empty() {
        prompt.push_str("- Surface focus summaries:\n");
        for (sid, summary) in &inputs.surface_summaries {
            prompt.push_str(&format!("  [{sid}] {summary}\n"));
        }
    }
    prompt.push_str(&format!("- Tool calls executed: {}\n", inputs.tool_call_count));

    if !inputs.cmux_surface_order.is_empty() {
        let order = inputs.cmux_surface_order.join(", ");
        prompt.push_str(&format!("Cmux surface order: [{order}]\n"));
    }

    prompt.push_str("\nProduce the new trajectory.md.\n");
    prompt
}
