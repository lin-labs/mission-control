use crate::mc_data::events::{Kind, Source};
use crate::mc_data::paths;
use anyhow::Result;
use std::collections::HashSet;

/// What the user has explicitly done to Goals & Progress items.
/// Used to overrule agent regen output that would undo human intent.
#[derive(Debug, Clone, Default)]
pub struct UserIntent {
    /// Task texts (normalized) the user has most recently CHECKED. Agent regen
    /// must not un-check these.
    pub human_checked: HashSet<String>,
    /// Task texts the user has most recently UNCHECKED. Agent regen must not
    /// re-check these.
    pub human_unchecked: HashSet<String>,
    /// Task texts the user has DELETED. Agent regen must not re-add them.
    pub human_deleted: HashSet<String>,
}

/// Normalize a task's display text for cross-event matching.
/// Drops:
///   - leading/trailing whitespace
///   - `[Pn]` priority prefix
///   - `[x]` / `[ ]` checkbox marker (defensive — items in our model don't
///     carry the marker inside `text`, but be safe)
///   - leading `- ` list marker
/// Then lowercases.
pub fn normalize_text(s: &str) -> String {
    let mut t = s.trim().to_string();
    // Strip leading `- ` list marker if present.
    if let Some(rest) = t.strip_prefix("- ") {
        t = rest.to_string();
    }
    // Strip checkbox marker if any leaked into `text`.
    if let Some(rest) = t.strip_prefix("[x] ").or_else(|| t.strip_prefix("[X] ")) {
        t = rest.to_string();
    } else if let Some(rest) = t.strip_prefix("[ ] ") {
        t = rest.to_string();
    }
    // Strip [Pn] priority prefix.
    let bytes = t.as_bytes();
    if bytes.len() >= 4
        && bytes[0] == b'['
        && (bytes[1] == b'P' || bytes[1] == b'p')
        && bytes[2].is_ascii_digit()
        && bytes[3] == b']'
    {
        // Drop "[Pn]" and optional following space.
        t = t[4..].trim_start().to_string();
    }
    t.to_lowercase()
}

/// Walk this workspace's events.jsonl and build a UserIntent that records
/// the latest human action on each Goals-section item.
///
/// "Latest" means: scan in append order; later events overwrite earlier
/// records for the same normalized text. Delete dominates uncheck dominates
/// check — but only because later events come last and overwrite previous
/// claims.
pub fn load_for_workspace(workspace_id: &str) -> Result<UserIntent> {
    let path = paths::events_log(workspace_id);
    let events = crate::mc_data::events::load(&path)?;
    let mut intent = UserIntent::default();
    for ev in events {
        // Only human events matter.
        let is_user = matches!(ev.source, Source::User);
        if !is_user {
            continue;
        }
        // Only events on the Goals & Progress section matter. Accept legacy
        // "Tasks & Progress" events too — older events.jsonl entries on disk
        // still use the pre-rename section label.
        let ev_section = ev.section.as_str();
        let is_goals_section = ev_section == crate::mc_data::trajectory::SECTION_GOALS
            || ev_section == "Tasks & Progress";
        if !is_goals_section {
            continue;
        }
        // The text to track: prefer `after` (post-edit), fall back to `before`.
        let text = ev
            .after
            .as_deref()
            .or(ev.before.as_deref())
            .map(normalize_text);
        let text = match text {
            Some(t) if !t.is_empty() => t,
            _ => continue,
        };
        match ev.kind {
            Kind::Check => {
                intent.human_checked.insert(text.clone());
                intent.human_unchecked.remove(&text);
                intent.human_deleted.remove(&text);
            }
            Kind::Uncheck => {
                intent.human_unchecked.insert(text.clone());
                intent.human_checked.remove(&text);
                intent.human_deleted.remove(&text);
            }
            Kind::Delete => {
                intent.human_deleted.insert(text.clone());
                intent.human_checked.remove(&text);
                intent.human_unchecked.remove(&text);
            }
            // Other kinds (add/edit/move) don't affect stickiness.
            _ => {}
        }
    }
    Ok(intent)
}

/// Apply the human intent to a freshly regenerated trajectory's Goals
/// section: force-check things the human checked, force-uncheck things the
/// human unchecked, drop things the human deleted.
pub fn apply_to_tasks(doc: &mut crate::mc_data::trajectory::TrajectoryDoc, intent: &UserIntent) {
    use crate::mc_data::trajectory::SECTION_GOALS;
    for s in doc.sections.iter_mut() {
        if s.name != SECTION_GOALS {
            continue;
        }
        s.items.retain(|item| {
            let n = normalize_text(&item.text);
            !intent.human_deleted.contains(&n)
        });
        for item in s.items.iter_mut() {
            let n = normalize_text(&item.text);
            if intent.human_checked.contains(&n) {
                item.is_checkbox = true;
                item.checked = Some(true);
            } else if intent.human_unchecked.contains(&n) {
                item.is_checkbox = true;
                item.checked = Some(false);
            }
        }
    }
}
