use crate::mc_data::paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub fn run(surface_id: &str, session_file: Option<&Path>) -> Result<()> {
    let workspace_id = match std::env::var("MC_WORKSPACE_ID") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("mc bind: MC_WORKSPACE_ID not set; skipping.");
            return Ok(());
        }
    };

    // Resolve the session file path. Priority:
    // 1. CLI --session-file arg
    // 2. $CLAUDE_SESSION_FILE env var
    // 3. Scan ~/agents/histories/ for latest matching file
    let path = match session_file {
        Some(p) => p.to_path_buf(),
        None => match std::env::var_os("CLAUDE_SESSION_FILE") {
            Some(p) => PathBuf::from(p),
            None => match fallback_scan_histories(&workspace_id) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("mc bind: could not locate session file: {e:#}; skipping.");
                    return Ok(());
                }
            },
        },
    };

    let surfaces_dir = paths::surfaces_dir(&workspace_id);
    std::fs::create_dir_all(&surfaces_dir)
        .with_context(|| format!("create surfaces dir {surfaces_dir:?}"))?;
    let pointer = surfaces_dir.join(format!("{surface_id}.session-path"));
    std::fs::write(&pointer, path.to_string_lossy().as_bytes())
        .with_context(|| format!("write pointer {pointer:?}"))?;
    Ok(())
}

/// Scan `~/agents/histories/*.md` for frontmatter `workspace_id: <uuid>` and
/// return the path of the file with the latest mtime that matches.
fn fallback_scan_histories(workspace_id: &str) -> Result<PathBuf> {
    let histories_dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("agents/histories");

    if !histories_dir.is_dir() {
        anyhow::bail!(
            "histories dir {} does not exist",
            histories_dir.display()
        );
    }

    let mut best: Option<(SystemTime, PathBuf)> = None;

    let read_dir =
        std::fs::read_dir(&histories_dir).with_context(|| format!("read {histories_dir:?}"))?;

    for entry in read_dir {
        let entry = entry?;
        let path = entry.path();

        // Only process .md files.
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        // Read just the first 2 KiB — enough to cover YAML frontmatter.
        let content = match read_first_bytes(&path, 2048) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if !frontmatter_contains_workspace_id(&content, workspace_id) {
            continue;
        }

        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => SystemTime::UNIX_EPOCH,
        };

        if best.as_ref().map_or(true, |(prev_t, _)| mtime > *prev_t) {
            best = Some((mtime, path));
        }
    }

    match best {
        Some((_, p)) => Ok(p),
        None => anyhow::bail!(
            "no history file found in {} matching workspace_id {}",
            histories_dir.display(),
            workspace_id
        ),
    }
}

fn read_first_bytes(path: &Path, limit: usize) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; limit];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Returns true if the content's YAML frontmatter contains a line like:
/// `workspace_id: <uuid>` where `<uuid>` matches `target`.
fn frontmatter_contains_workspace_id(content: &str, target: &str) -> bool {
    // Find frontmatter delimited by `---` lines.
    let mut lines = content.lines();

    // The first non-empty line must be `---` to be valid frontmatter.
    let first = lines.next().unwrap_or("").trim();
    if first != "---" {
        return false;
    }

    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break; // end of frontmatter
        }
        // Match `workspace_id: <value>` (possibly with surrounding whitespace)
        if let Some(rest) = trimmed.strip_prefix("workspace_id:") {
            if rest.trim() == target {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_matches_workspace_id() {
        let content = "---\nworkspace_id: abc-123\ntitle: test\n---\n# body\n";
        assert!(frontmatter_contains_workspace_id(content, "abc-123"));
        assert!(!frontmatter_contains_workspace_id(content, "other-id"));
    }

    #[test]
    fn frontmatter_no_match_without_delimiter() {
        let content = "workspace_id: abc-123\ntitle: test\n";
        assert!(!frontmatter_contains_workspace_id(content, "abc-123"));
    }
}
