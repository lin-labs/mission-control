use anyhow::Result;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Context about the workspace being looked up, used to disambiguate session
/// logs when multiple workspaces share the same host or when a surface's cwd
/// differs from the workspace's registered directory.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceContext {
    /// Short hostname (e.g. "mbp", "labs-devbox"). Case-insensitive on compare.
    pub host: Option<String>,
    /// Current working directory of the cmux workspace (from `current_directory`
    /// field in cmux JSON output). Used for cwd-prefix matching against session
    /// log frontmatter.
    pub cwd: Option<String>,
}

/// Parsed YAML frontmatter of a session log file.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Frontmatter {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

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

/// Try to parse a `## <time> [PT] [--|-] <role>` heading line.
/// Returns `Some((time, role))` on success, `None` otherwise.
fn parse_turn_heading(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("## ")?;

    // Split on em-dash or regular hyphen surrounded by optional spaces.
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
        // A line that is exactly `---` is a turn separator -- skip it.
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

/// Shortcut: return the content of the most recent `## <time> -- boyan` block,
/// or None if no user turn exists.
pub fn last_user_turn(text: &str) -> Option<String> {
    let turns = parse(text);
    turns
        .into_iter()
        .rev()
        .find(|t| t.role == "boyan")
        .map(|t| t.content)
}

/// Scan ~obsAgents/Sessions/*.md and return the path of the best-matching
/// session log for the given workspace, using a two-tier algorithm:
///
/// **Tier 1 (host + cwd match -- strongest signal)**
/// Filter candidates where:
/// - `fm_host` matches `ctx.host` (case-insensitive)
/// - `fm_cwd` is a descendant of (or equal to) `ctx.cwd` via path-prefix match
///
/// Among tier-1 candidates, pick the one with the most-specific (deepest) cwd.
/// Tie-break within the same specificity level by mtime (newest wins).
///
/// **Tier 2 (workspace_id fallback -- backward compat)**
/// If tier 1 yields nothing AND `workspace_id` is non-empty, filter by
/// `fm_workspace_id == workspace_id`. Return newest by mtime.
///
/// Returns `Ok(None)` if no matching file is found.
pub fn latest_session_file_for_workspace(
    workspace_id: &str,
    ctx: &WorkspaceContext,
) -> Result<Option<PathBuf>> {
    let sessions_dir = crate::mc_data::prompts::obsagents_root().join("Sessions");

    if !sessions_dir.exists() {
        return Ok(None);
    }

    // Build candidate list.
    struct Candidate {
        path: PathBuf,
        mtime: std::time::SystemTime,
        fm: Frontmatter,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    let entries = std::fs::read_dir(&sessions_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let fm = parse_frontmatter(&text);
        candidates.push(Candidate { path, mtime, fm });
    }

    // Tier 1: host + cwd match.
    // Only active when ctx has both host and cwd set.
    if let (Some(ctx_host), Some(ctx_cwd)) = (&ctx.host, &ctx.cwd) {
        let ctx_host_norm = normalize_host(ctx_host);

        let tier1: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| {
                let host_ok = c
                    .fm
                    .host
                    .as_deref()
                    .map(|h| normalize_host(h) == ctx_host_norm)
                    .unwrap_or(false);
                let cwd_ok = c
                    .fm
                    .cwd
                    .as_deref()
                    .map(|fc| is_descendant(fc, ctx_cwd))
                    .unwrap_or(false);
                host_ok && cwd_ok
            })
            .collect();

        if !tier1.is_empty() {
            // Pick the most-specific cwd (highest depth), tie-break by newest mtime.
            let best = tier1
                .into_iter()
                .max_by(|a, b| {
                    let da = a.fm.cwd.as_deref().map(cwd_depth).unwrap_or(0);
                    let db = b.fm.cwd.as_deref().map(cwd_depth).unwrap_or(0);
                    da.cmp(&db).then_with(|| a.mtime.cmp(&b.mtime))
                })
                .unwrap(); // safe: tier1 is non-empty
            return Ok(Some(best.path.clone()));
        }
    }

    // Tier 2: workspace_id fallback (backward compat with old logs that have no
    // host/cwd tags).  We intentionally do NOT require a host match here so that
    // logs written before host tagging was introduced continue to work.
    if !workspace_id.is_empty() {
        let mut best: Option<(std::time::SystemTime, &PathBuf)> = None;
        for c in &candidates {
            if c.fm.workspace_id.as_deref() == Some(workspace_id) {
                if best.as_ref().map_or(true, |(bt, _)| c.mtime > *bt) {
                    best = Some((c.mtime, &c.path));
                }
            }
        }
        if let Some((_, p)) = best {
            return Ok(Some(p.clone()));
        }
    }

    Ok(None)
}

/// Resolve the session-log file for a given workspace + surface.
///
/// Two-step lookup:
/// 1. Per-surface pointer file: `<surfaces_dir>/<surface_id>.session-path`.
///    If it exists and its content points to an existing file, return that path.
/// 2. Workspace-level fallback: `latest_session_file_for_workspace` with the
///    provided `WorkspaceContext` for host+cwd disambiguation.
///
/// Returns `Ok(None)` when neither lookup finds a file (Shell source).
pub fn resolve_session_log_for_surface(
    workspace_uuid: &str,
    surface_id: &str,
    ctx: &WorkspaceContext,
) -> Result<Option<PathBuf>> {
    // Step 1: per-surface pointer file.
    let pointer = crate::mc_data::paths::surfaces_dir(workspace_uuid)
        .join(format!("{surface_id}.session-path"));
    if let Ok(content) = std::fs::read_to_string(&pointer) {
        let p = PathBuf::from(content.trim());
        if p.exists() {
            return Ok(Some(p));
        }
    }
    // Step 2: workspace-level fallback (with host+cwd disambiguation).
    latest_session_file_for_workspace(workspace_uuid, ctx)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse the YAML frontmatter of a session log file into a `Frontmatter` struct.
/// Returns a default (all-None) `Frontmatter` on any parse failure.
fn parse_frontmatter(text: &str) -> Frontmatter {
    let (fm, _) = split_frontmatter(text);
    if fm.is_empty() {
        return Frontmatter::default();
    }
    serde_yaml::from_str(fm).unwrap_or_default()
}

/// Returns true if `child` is a path descendant of (or equal to) `ancestor`.
/// Both paths are treated as POSIX strings; trailing slashes are stripped.
fn is_descendant(child: &str, ancestor: &str) -> bool {
    let c = child.trim_end_matches('/');
    let a = ancestor.trim_end_matches('/');
    c == a || c.starts_with(&format!("{a}/"))
}

/// Count path separators as a rough measure of cwd specificity.
/// Higher = more specific = preferred in tier-1 selection.
fn cwd_depth(p: &str) -> usize {
    p.matches('/').count()
}

/// Normalize a hostname for case-insensitive comparison: lowercase + strip trailing dots.
fn normalize_host(h: &str) -> String {
    h.trim_end_matches('.').to_ascii_lowercase()
}
