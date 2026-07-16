use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Arcmux publishes immutable handoff transport snapshots beside canonical
/// conversation histories so the mesh sync can carry them. They are protocol
/// artifacts, not conversations, and must never enter Mission Control's
/// session discovery or trajectory inputs.
pub const HANDOFF_TRANSPORT_HISTORY_PREFIX: &str = "arcmux-handoff-sha256-";

pub fn is_canonical_history_path(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "md")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                !name.starts_with('.') && !name.starts_with(HANDOFF_TRANSPORT_HISTORY_PREFIX)
            })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frontmatter {
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionFile {
    pub path: PathBuf,
    pub frontmatter: Frontmatter,
    pub bullets: Vec<String>,
    pub trajectory: Option<String>,
    pub next_steps: Vec<String>,
    pub other_body: String,
}

impl SessionFile {
    pub fn parse(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse_str(&content, path.to_path_buf())
    }

    pub fn parse_str(content: &str, path: PathBuf) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(content)?;

        let mut bullets = Vec::new();
        let mut trajectory = None;
        let mut next_steps = Vec::new();
        let mut other_body = String::new();
        let mut section: Option<&str> = None;

        for line in body.lines() {
            if line.starts_with("## Trajectory") {
                section = Some("trajectory");
                continue;
            } else if line.starts_with("## Next Steps") {
                section = Some("next_steps");
                continue;
            } else if line.starts_with("## ") {
                section = Some("other");
            }

            match section {
                None => {
                    let trimmed = line.trim();
                    if trimmed.starts_with("- ")
                        && !trimmed.starts_with("- [ ]")
                        && !trimmed.starts_with("- [x]")
                    {
                        bullets.push(trimmed.trim_start_matches("- ").to_string());
                    } else if !trimmed.is_empty() {
                        other_body.push_str(line);
                        other_body.push('\n');
                    }
                }
                Some("trajectory") => {
                    let trimmed = line.trim().trim_start_matches("> ").trim();
                    if !trimmed.is_empty() {
                        trajectory = Some(trimmed.to_string());
                    }
                }
                Some("next_steps") => {
                    let trimmed = line.trim();
                    if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") {
                        next_steps.push(trimmed.to_string());
                    }
                }
                _ => {
                    other_body.push_str(line);
                    other_body.push('\n');
                }
            }
        }

        Ok(SessionFile {
            path,
            frontmatter,
            bullets,
            trajectory,
            next_steps,
            other_body,
        })
    }

    pub fn to_markdown(&self) -> Result<String> {
        let yaml = serde_yaml::to_string(&self.frontmatter)?;
        let mut out = format!("---\n{}---\n\n", yaml);

        for bullet in &self.bullets {
            out.push_str(&format!("- {}\n", bullet));
        }

        if let Some(ref traj) = self.trajectory {
            out.push_str(&format!("\n## Trajectory\n> {}\n", traj));
        }

        if !self.next_steps.is_empty() {
            out.push_str("\n## Next Steps\n");
            for step in &self.next_steps {
                out.push_str(&format!("{}\n", step));
            }
        }

        if !self.other_body.trim().is_empty() {
            out.push('\n');
            out.push_str(self.other_body.trim());
            out.push('\n');
        }

        Ok(out)
    }

    pub fn write(&self) -> Result<()> {
        let content = self.to_markdown()?;
        std::fs::write(&self.path, content)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

fn split_frontmatter(content: &str) -> Result<(Frontmatter, String)> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return Ok((Frontmatter::default(), content.to_string()));
    }

    let after_first = &content[3..];
    let end = after_first
        .find("\n---")
        .context("no closing --- in frontmatter")?;

    let yaml_str = &after_first[..end];
    let body = &after_first[end + 4..];

    let frontmatter: Frontmatter = serde_yaml::from_str(yaml_str.trim()).unwrap_or_default();

    Ok((frontmatter, body.trim_start_matches('\n').to_string()))
}

#[allow(dead_code)] // superseded in the binary by `list_recent_session_files`; integration tests still exercise it.
pub fn list_session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_canonical_history_path(p))
        .collect();

    files.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });

    Ok(files)
}

/// List session files whose filename starts with a date prefix in the last
/// `days` days. Purely filename-based — does NOT stat each file, so the cost
/// is one `read_dir` syscall regardless of how many old session logs the
/// directory contains.
///
/// Per the AGENTS.md "Session History Logging" convention each file is named
/// `YYYY-MM-DD-HH-slug.md` (Pacific Time at session start). String comparison
/// works as chronological comparison for the `YYYY-MM-DD` prefix.
///
/// Files are returned sorted by filename **descending** (newest first), which
/// for this naming convention is equivalent to most-recent-first.
pub fn list_recent_session_files(dir: &Path, days: i64) -> Result<Vec<PathBuf>> {
    let cutoff = (chrono::Local::now() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            if !is_canonical_history_path(p) {
                return false;
            }
            // Filename must start with a YYYY-MM-DD prefix that's >= cutoff.
            // Lexicographic comparison works because the prefix is fixed-width
            // ISO 8601 date.
            name.len() >= 10
                && name.as_bytes()[4] == b'-'
                && name.as_bytes()[7] == b'-'
                && &name[..10] >= cutoff.as_str()
        })
        .collect();

    // Sort by filename descending — newest first, no stat() calls needed.
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_transport_markdown_is_not_a_canonical_history() {
        assert!(!is_canonical_history_path(Path::new(
            "arcmux-handoff-sha256-0123456789abcdef.md"
        )));
        assert!(!is_canonical_history_path(Path::new(".hidden.md")));
        assert!(is_canonical_history_path(Path::new(
            "2026-07-15-20-surface-handoff.md"
        )));
    }

    #[test]
    fn both_session_scans_exclude_handoff_transport_markdown() {
        let temp = tempfile::tempdir().unwrap();
        let canonical = temp.path().join(format!(
            "{}-canonical.md",
            chrono::Local::now().format("%Y-%m-%d-%H")
        ));
        let transport = temp
            .path()
            .join("arcmux-handoff-sha256-0123456789abcdef.md");
        std::fs::write(&canonical, "canonical").unwrap();
        std::fs::write(&transport, "transport").unwrap();

        assert_eq!(
            list_session_files(temp.path()).unwrap(),
            vec![canonical.clone()]
        );
        assert_eq!(
            list_recent_session_files(temp.path(), 1).unwrap(),
            vec![canonical]
        );
    }
}
