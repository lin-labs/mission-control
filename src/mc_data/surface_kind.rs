//! Per-surface kind detection (Claude / Codex / Shell / …).
//!
//! Detection strategy
//! ------------------
//!
//! 1. `lsof -t /dev/ttysNNN` lists every process attached to the surface's
//!    controlling terminal — usually the login shell, the user shell, and any
//!    descendants (e.g. a running `claude` CLI). On macOS, lsof prints one
//!    PID per line.
//!
//! 2. For each PID we ask `ps -p <pid> -o stat=,comm=` for its state and the
//!    program name. macOS marks the foreground process group with a trailing
//!    `+` in the STAT column. That `+` is the most reliable signal that the
//!    process is the one currently driving the terminal — i.e. what the user
//!    is interacting with.
//!
//! 3. We classify the foreground process's basename via `SurfaceKind::from_comm`.
//!    If no PID is in the foreground (race or short-lived state), we fall back
//!    to the *last* PID lsof returned, which is empirically the deepest
//!    descendant on macOS.
//!
//! The whole pipeline is best-effort: any failure returns `SurfaceKind::Unknown`
//! so detection never blocks the TUI refresh tick.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Claude,
    Codex,
    OtherAgent,
    Shell,
    Unknown,
}

impl SurfaceKind {
    // `glyph` and `label` are wired in by T3 (sidebar rendering). They're
    // already exercised by the integration tests under `tests/surface_kind.rs`,
    // but the binary target alone hasn't called them yet — silence the bin
    // dead-code lint until T3 lands.
    #[allow(dead_code)]
    pub fn glyph(self) -> char {
        match self {
            Self::Claude => '✻',
            Self::Codex => '▲',
            Self::OtherAgent => '◆',
            Self::Shell => '$',
            Self::Unknown => '·',
        }
    }

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OtherAgent => "agent",
            Self::Shell => "shell",
            Self::Unknown => "?",
        }
    }

    /// Classify a `ps -o comm=` value. `comm` may arrive as either a basename
    /// (`zsh`), a full path (`/opt/homebrew/bin/claude`), or a login-shell-style
    /// path with a leading dash (`-/bin/zsh`). Strip path components and any
    /// leading dash before matching.
    pub fn from_comm(comm: &str) -> Self {
        let trimmed = comm.trim();
        // Drop login-shell leading '-' (e.g. "-/bin/zsh" or "-zsh").
        let no_dash = trimmed.strip_prefix('-').unwrap_or(trimmed);
        let basename = no_dash
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .trim();
        match basename {
            "claude" => Self::Claude,
            "codex" => Self::Codex,
            "cursor-agent" | "aider" | "goose" => Self::OtherAgent,
            "zsh" | "bash" | "fish" | "sh" => Self::Shell,
            _ => Self::Unknown,
        }
    }

    pub fn is_agent(self) -> bool {
        matches!(self, Self::Claude | Self::Codex | Self::OtherAgent)
    }
}

impl Default for SurfaceKind {
    fn default() -> Self {
        Self::Unknown
    }
}

/// Detect the surface kind for a given tty path.
///
/// `tty` may be `"ttys030"` or `"/dev/ttys030"`. Returns `Unknown` on any error
/// — this function must never panic and must never block the caller for long.
pub fn detect(tty: &str) -> SurfaceKind {
    let dev_path = if tty.starts_with("/dev/") {
        tty.to_string()
    } else {
        format!("/dev/{}", tty)
    };

    // Step 1: lsof -t /dev/ttysNNN → list of pids, one per line.
    let lsof = std::process::Command::new("lsof")
        .args(["-t", &dev_path])
        .output();
    let Ok(lsof) = lsof else {
        return SurfaceKind::Unknown;
    };
    if !lsof.status.success() {
        return SurfaceKind::Unknown;
    }

    let pids: Vec<u32> = String::from_utf8_lossy(&lsof.stdout)
        .lines()
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    if pids.is_empty() {
        return SurfaceKind::Unknown;
    }

    // Step 2: ask ps about each pid; prefer the foreground (`+` in STAT).
    // Fall back to the last pid lsof returned if none is foreground.
    let mut foreground: Option<String> = None;
    let mut fallback: Option<String> = None;
    for pid in &pids {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat=,comm="])
            .output();
        let Ok(out) = out else { continue };
        if !out.status.success() {
            continue;
        }
        let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if line.is_empty() {
            continue;
        }
        // ps with two `=`-named columns emits e.g. "S+   /opt/.../claude".
        // Split on first whitespace: STAT then COMM (which may contain spaces
        // in pathological cases — preserve the rest verbatim).
        let (stat, comm) = match line.split_once(char::is_whitespace) {
            Some((s, c)) => (s, c.trim_start()),
            None => continue,
        };
        if stat.contains('+') {
            foreground = Some(comm.to_string());
            break;
        }
        fallback = Some(comm.to_string());
    }

    let comm = match foreground.or(fallback) {
        Some(c) => c,
        None => return SurfaceKind::Unknown,
    };
    SurfaceKind::from_comm(&comm)
}

// ── Last-agent persistence ─────────────────────────────────────────────────

/// A snapshot of the most recent agent kind seen on a surface, with the time
/// the snapshot was taken. Persisted as JSON next to the workspace data so a
/// recently-exited agent still renders with its agent glyph for ~5 minutes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastAgent {
    pub kind: SurfaceKind,
    pub ts: chrono::DateTime<chrono::Utc>,
}

/// Sanitize a surface ref like `"surface:92"` into a safe filename stem.
/// Colons are legal on APFS but better avoided for portability.
fn surface_ref_to_filename(surface_ref: &str) -> String {
    surface_ref.replace(['/', '\\', ':'], "_")
}

fn last_agent_path(workspace_uuid: &str, surface_ref: &str) -> PathBuf {
    crate::mc_data::paths::surfaces_dir(workspace_uuid)
        .join(format!("{}.last-agent", surface_ref_to_filename(surface_ref)))
}

/// Persist `kind` as the last-known agent for this surface. No-op when the
/// kind is not an agent (we don't want a Shell heartbeat to clobber a recent
/// Claude snapshot).
pub fn write_last_agent(
    workspace_uuid: &str,
    surface_ref: &str,
    kind: SurfaceKind,
) -> anyhow::Result<()> {
    if !kind.is_agent() {
        return Ok(());
    }
    let path = last_agent_path(workspace_uuid, surface_ref);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let snapshot = LastAgent {
        kind,
        ts: chrono::Utc::now(),
    };
    let body = serde_json::to_string(&snapshot)?;
    // Atomic-ish write: tmp file in same dir, then rename.
    let tmp = path.with_extension("last-agent.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the last-agent snapshot for a surface, if any. Returns `None` when
/// the file is missing, unreadable, or invalid JSON.
#[allow(dead_code)] // wired into rendering by T3
pub fn read_last_agent(workspace_uuid: &str, surface_ref: &str) -> Option<LastAgent> {
    let path = last_agent_path(workspace_uuid, surface_ref);
    let body = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Resolve the kind to render for this surface. If the currently detected
/// kind is an agent, use it directly. Otherwise consult the last-agent file:
/// if it was written within the last 5 minutes, surface that agent kind so
/// the glyph doesn't flip back to Shell the instant the agent exits.
#[allow(dead_code)] // wired into rendering by T3
pub fn effective_kind(
    workspace_uuid: &str,
    surface_ref: &str,
    current: SurfaceKind,
) -> SurfaceKind {
    if current.is_agent() {
        return current;
    }
    if let Some(last) = read_last_agent(workspace_uuid, surface_ref) {
        let age = chrono::Utc::now() - last.ts;
        if age.num_seconds() < 300 {
            return last.kind;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_comm_maps_known_agents() {
        assert_eq!(SurfaceKind::from_comm("claude"), SurfaceKind::Claude);
        assert_eq!(SurfaceKind::from_comm("codex"), SurfaceKind::Codex);
        assert_eq!(
            SurfaceKind::from_comm("cursor-agent"),
            SurfaceKind::OtherAgent
        );
        assert_eq!(SurfaceKind::from_comm("aider"), SurfaceKind::OtherAgent);
        assert_eq!(SurfaceKind::from_comm("goose"), SurfaceKind::OtherAgent);
    }

    #[test]
    fn from_comm_handles_paths_and_login_dash() {
        assert_eq!(
            SurfaceKind::from_comm("/opt/homebrew/bin/claude"),
            SurfaceKind::Claude
        );
        assert_eq!(SurfaceKind::from_comm("-/bin/zsh"), SurfaceKind::Shell);
        assert_eq!(SurfaceKind::from_comm("-zsh"), SurfaceKind::Shell);
        assert_eq!(SurfaceKind::from_comm("  zsh\n"), SurfaceKind::Shell);
    }

    #[test]
    fn unknown_falls_through() {
        assert_eq!(SurfaceKind::from_comm("vim"), SurfaceKind::Unknown);
        assert_eq!(SurfaceKind::from_comm(""), SurfaceKind::Unknown);
    }
}
