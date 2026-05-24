use anyhow::Result;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SessionTurn {
    pub time: String,    // "17:30 PT"
    pub role: String,    // "boyan", "claude", "codex", ...
    pub content: String, // verbatim block content (excluding the heading line)
}

// ---------------------------------------------------------------------------
// Heading parser
// ---------------------------------------------------------------------------

/// Try to parse a `## <time> [PT] [—|-] <role>` heading line.
/// Returns `Some((time, role))` on success, `None` otherwise.
fn parse_turn_heading(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("## ")?;

    // Split on em-dash (—) or regular hyphen surrounded by optional spaces.
    // em-dash is U+2014 (3 bytes in UTF-8: "\xe2\x80\x94")
    let (time_part, role_part) = if let Some(idx) = rest.find('\u{2014}') {
        (&rest[..idx], &rest[idx + '\u{2014}'.len_utf8()..])
    } else if let Some(idx) = rest.find(" - ") {
        (&rest[..idx], &rest[idx + 3..])
    } else {
        return None;
    };

    let time = time_part.trim().to_string();
    let role = role_part.trim().to_string();

    if time.is_empty() || role.is_empty() {
        return None;
    }

    Some((time, role))
}

// ---------------------------------------------------------------------------
// split_frontmatter (local copy, same logic as trajectory.rs)
// ---------------------------------------------------------------------------

fn split_frontmatter(text: &str) -> (&str, &str) {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let after = &rest[end + 4..];
            let body = after.strip_prefix('\n').unwrap_or(after);
            return (fm, body);
        }
    }
    ("", text)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse all turns from a session-log file's text.
pub fn parse(text: &str) -> Vec<SessionTurn> {
    let (_, body) = split_frontmatter(text);

    let mut turns: Vec<SessionTurn> = Vec::new();
    let mut current: Option<(String, String, Vec<String>)> = None; // (time, role, lines)

    for line in body.lines() {
        // A line that is exactly `---` is a turn separator — skip it.
        if line.trim() == "---" {
            continue;
        }

        // Check for a new turn heading.
        if let Some((time, role)) = parse_turn_heading(line) {
            // Flush the previous turn.
            if let Some((t, r, lines)) = current.take() {
                turns.push(SessionTurn {
                    time: t,
                    role: r,
                    content: lines.join("\n"),
                });
            }
            current = Some((time, role, Vec::new()));
            continue;
        }

        // Accumulate content lines.
        if let Some((_, _, ref mut lines)) = current {
            lines.push(line.to_string());
        }
    }

    // Flush the last open turn.
    if let Some((t, r, lines)) = current.take() {
        turns.push(SessionTurn {
            time: t,
            role: r,
            content: lines.join("\n"),
        });
    }

    turns
}

/// Shortcut: return the content of the most recent `## <time> — boyan` block,
/// or None if no user turn exists.
pub fn last_user_turn(text: &str) -> Option<String> {
    let turns = parse(text);
    turns
        .into_iter()
        .rev()
        .find(|t| t.role == "boyan")
        .map(|t| t.content)
}

/// Scan ~obsAgents/Sessions/*.md and return the path of the most-recently-
/// modified file whose frontmatter `workspace_id` matches `workspace_id`.
///
/// Returns Ok(None) if no matching file exists.
pub fn latest_session_file_for_workspace(workspace_id: &str) -> Result<Option<PathBuf>> {
    let sessions_dir = crate::mc_data::prompts::obsagents_root().join("Sessions");

    if !sessions_dir.exists() {
        return Ok(None);
    }

    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;

    let entries = std::fs::read_dir(&sessions_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        // Read the file and check workspace_id in frontmatter.
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if !frontmatter_workspace_id_matches(&text, workspace_id) {
            continue;
        }

        // Get modification time.
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        if best.as_ref().map_or(true, |(best_time, _)| mtime > *best_time) {
            best = Some((mtime, path));
        }
    }

    Ok(best.map(|(_, p)| p))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns true if the YAML frontmatter of `text` contains a `workspace_id`
/// field equal to `target`.
fn frontmatter_workspace_id_matches(text: &str, target: &str) -> bool {
    let (fm, _) = split_frontmatter(text);
    if fm.is_empty() {
        return false;
    }
    // Use serde_yaml to parse the frontmatter.
    let value: serde_yaml::Value = match serde_yaml::from_str(fm) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if let serde_yaml::Value::Mapping(map) = value {
        if let Some(v) = map.get(serde_yaml::Value::String("workspace_id".to_string())) {
            if let serde_yaml::Value::String(s) = v {
                return s == target;
            }
        }
    }
    false
}
