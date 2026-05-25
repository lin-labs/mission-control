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
    /// Which agent wrote this log: "claude", "codex", "opencode", etc. Older
    /// logs (pre-tagging) leave this absent — callers must treat missing as
    /// "match any agent" so they don't silently drop those files.
    #[serde(default)]
    pub agent: Option<String>,
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

/// Scan ~obsAgents/Sessions/*.md and return ALL matching session logs for the
/// given workspace, using a two-tier algorithm.  Results are sorted by mtime
/// descending (newest first).
///
/// **Tier 1 (host + cwd match -- strongest signal)**
/// Filter candidates where:
/// - `fm_host` matches `ctx.host` (case-insensitive)
/// - `fm_cwd` is a descendant of (or equal to) `ctx.cwd` via path-prefix match
///
/// Among tier-1 candidates, sort by most-specific cwd (deepest) first, then
/// by newest mtime as a tie-breaker.
///
/// **Tier 2 (workspace_id fallback -- backward compat)**
/// If tier 1 yields nothing AND `workspace_id` is non-empty, filter by
/// `fm_workspace_id == workspace_id`. Sort newest-first by mtime.
///
/// Returns `Ok(vec![])` if no matching files are found.
pub fn matching_session_files_for_workspace(
    workspace_id: &str,
    ctx: &WorkspaceContext,
) -> Result<Vec<PathBuf>> {
    let sessions_dir = crate::mc_data::prompts::obsagents_root().join("Sessions");

    if !sessions_dir.exists() {
        return Ok(Vec::new());
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
    // Only active when ctx has both host and cwd set AND ctx.cwd is specific
    // enough (not the user's home dir or shallower). cwd = $HOME would match
    // every session log Boyan ever wrote, so we skip tier-1 in that case and
    // let tier-2 (workspace_id match) decide.
    if let (Some(ctx_host), Some(ctx_cwd)) = (&ctx.host, &ctx.cwd) {
        if !cwd_is_too_shallow(ctx_cwd) {
            let mut tier1: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| {
                    let host_ok = c
                        .fm
                        .host
                        .as_deref()
                        .map(|h| hosts_match(h, ctx_host))
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
                // Sort by most-specific cwd (deepest) first, tie-break by newest mtime.
                tier1.sort_by(|a, b| {
                    let da = a.fm.cwd.as_deref().map(cwd_depth).unwrap_or(0);
                    let db = b.fm.cwd.as_deref().map(cwd_depth).unwrap_or(0);
                    db.cmp(&da).then_with(|| b.mtime.cmp(&a.mtime))
                });
                return Ok(tier1.into_iter().map(|c| c.path.clone()).collect());
            }
        }
    }

    // Tier 2: workspace_id fallback (backward compat with old logs that have no
    // host/cwd tags).  We intentionally do NOT require a host match here so that
    // logs written before host tagging was introduced continue to work.
    if !workspace_id.is_empty() {
        let mut tier2: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.fm.workspace_id.as_deref() == Some(workspace_id))
            .collect();
        if !tier2.is_empty() {
            tier2.sort_by(|a, b| b.mtime.cmp(&a.mtime));
            return Ok(tier2.into_iter().map(|c| c.path.clone()).collect());
        }
    }

    Ok(Vec::new())
}

/// Scan ~obsAgents/Sessions/*.md and return the path of the best-matching
/// session log for the given workspace.  This is a convenience wrapper around
/// [`matching_session_files_for_workspace`] that returns only the first (newest)
/// match.
///
/// Returns `Ok(None)` if no matching file is found.
pub fn latest_session_file_for_workspace(
    workspace_id: &str,
    ctx: &WorkspaceContext,
) -> Result<Option<PathBuf>> {
    Ok(matching_session_files_for_workspace(workspace_id, ctx)?
        .into_iter()
        .next())
}

/// Resolve the session-log file for a given workspace surface.
///
/// Three-step lookup:
/// 1. Per-surface pointer file: `<surfaces_dir>/<surface_id>.session-path`.
///    If it exists and its content points to an existing file, return that path.
/// 2. Collect workspace-matched logs (host+cwd or workspace_uuid tier).
///    When `agent_label` is `Some("claude" | "codex" | …)`, filter to logs
///    whose frontmatter `agent` field matches; logs without an `agent` field
///    are kept as candidates for any agent kind (pre-tagging back-compat).
///    When `agent_label` is `None` (surface kind couldn't be detected), no
///    agent filter is applied — fall back to the workspace-wide candidate
///    list so the peek shows *something* relevant.
///    Distribute the resulting list across same-kind siblings by
///    `same_agent_index`; overflow returns the oldest match.
/// 3. Return `None` only when no candidate files exist after the filter.
///
/// **Shell surfaces should not call this function.** Callers know their
/// surface kind and force `PeekSource::Shell` directly for shells; this
/// function exists only for peeks that *might* have a backing session log.
pub fn resolve_session_log_for_surface(
    workspace_uuid: &str,
    surface_id: &str,
    ctx: &WorkspaceContext,
    agent_label: Option<&str>,
    same_agent_index: usize,
) -> Result<Option<PathBuf>> {
    // Step 1: per-surface pointer file (works for any surface kind).
    let pointer = crate::mc_data::paths::surfaces_dir(workspace_uuid)
        .join(format!("{surface_id}.session-path"));
    if let Ok(content) = std::fs::read_to_string(&pointer) {
        let p = PathBuf::from(content.trim());
        if p.exists() {
            return Ok(Some(p));
        }
    }

    // Step 2: collect matches, optionally filter by agent, then index.
    let matches = matching_session_files_for_workspace(workspace_uuid, ctx)?;
    if matches.is_empty() {
        return Ok(None);
    }

    let candidates: Vec<PathBuf> = match agent_label {
        Some(label) => matches
            .into_iter()
            .filter(|p| {
                let text = match std::fs::read_to_string(p) {
                    Ok(t) => t,
                    Err(_) => return false,
                };
                let fm = parse_frontmatter(&text);
                // Missing agent: keep as candidate (pre-tagging compatibility).
                // Present agent: must match this surface's kind.
                fm.agent
                    .as_deref()
                    .map(|a| a.eq_ignore_ascii_case(label))
                    .unwrap_or(true)
            })
            .collect(),
        None => matches,
    };

    if candidates.is_empty() {
        return Ok(None);
    }
    if same_agent_index < candidates.len() {
        Ok(Some(candidates[same_agent_index].clone()))
    } else {
        Ok(candidates.into_iter().last())
    }
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

/// Fuzzy hostname match: equal after normalization, or one is a substring of
/// the other. Real-world reason: Boyan's machine's `hostname -s` is
/// `blin-mbp` but his session logs are tagged `host: mbp`. Strict equality
/// would never match.
fn hosts_match(a: &str, b: &str) -> bool {
    let na = normalize_host(a);
    let nb = normalize_host(b);
    if na == nb {
        return true;
    }
    // Substring in either direction — `mbp` matches `blin-mbp` and vice versa.
    !na.is_empty() && !nb.is_empty() && (na.contains(&nb) || nb.contains(&na))
}

/// `cwd` too broad to be useful for matching session files. We treat the
/// user's home dir (or any path shallower than `/Users/<name>/<one-level>`)
/// as "too shallow" — matching by it would pull in every session log under
/// $HOME, which is everything Boyan has ever written.
fn cwd_is_too_shallow(cwd: &str) -> bool {
    let trimmed = cwd.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return true;
    }
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        let home_trim = home_str.trim_end_matches('/');
        if trimmed == home_trim {
            return true;
        }
    }
    // Depth heuristic: fewer than 3 path segments under root is too shallow
    // (e.g., `/Users`, `/Users/blin`).
    cwd_depth(trimmed) < 3
}
