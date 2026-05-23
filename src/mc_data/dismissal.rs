use crate::mc_data::{paths, workspace};
use anyhow::{Context, Result};
use chrono::Local;
use std::fs;
use std::path::PathBuf;

pub struct DismissalArtifacts {
    pub local_archive: PathBuf,          // .archived/<date>-<name>/
    pub obsidian_record: PathBuf,        // ~obsAgents/mc-workspaces/<date>-<name>.md
    pub proposals_file: Option<PathBuf>, // ~obsAgents/Projects/<project>/prompts/proposals/...
}

/// Publish the learning artifact + move local data into .archived/.
/// Pure file-shuffling — the LLM call (producing learning.md and the proposal
/// candidates) is the caller's responsibility.
///
/// Safety: steps 1–3 (write + publish) are non-destructive. Step 4 (symlink
/// removal) and step 5 (atomic rename) are the point-of-no-return.  If steps
/// 1–3 fail we return Err _before_ touching step 4/5, so the data dir stays
/// intact and the caller can retry.
pub fn finalize(
    uuid: &str,
    learning_md_content: &str,
    proposal_md_content: Option<&str>,
) -> Result<DismissalArtifacts> {
    let display_name = workspace::read_display_name(uuid)?;
    let project = workspace::read_project(uuid).unwrap_or_else(|_| display_name.clone());
    let date = Local::now().format("%Y-%m-%d").to_string();

    // 1. Write learning.md locally (inside the data dir, before archive move).
    let data_dir = paths::workspace_dir(uuid);
    let learning_path = data_dir.join("learning.md");
    fs::write(&learning_path, learning_md_content)
        .with_context(|| format!("write {learning_path:?}"))?;

    // 2. Publish to obsidian: ~obsAgents/mc-workspaces/<date>-<name>.md
    let obs_root = crate::mc_data::prompts::obsagents_root();
    let mc_ws_dir = obs_root.join("mc-workspaces");
    fs::create_dir_all(&mc_ws_dir)
        .with_context(|| format!("create obsidian mc-workspaces dir {mc_ws_dir:?}"))?;
    let obsidian_record = mc_ws_dir.join(format!("{date}-{display_name}.md"));
    fs::write(&obsidian_record, learning_md_content)
        .with_context(|| format!("write obsidian record {obsidian_record:?}"))?;

    // 3. Proposals file (non-fatal inner; if a write fails we skip the file but
    //    do not abort the whole dismissal — the full record is still in obsidian).
    let proposals_file = proposal_md_content.map(|content| {
        let dir = obs_root
            .join("Projects")
            .join(&project)
            .join("prompts")
            .join("proposals");
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("dismissal: create proposals dir {dir:?}: {e}");
            return None;
        }
        let path = dir.join(format!("{date}-{display_name}.md"));
        if let Err(e) = fs::write(&path, content) {
            eprintln!("dismissal: write proposals {path:?}: {e}");
            return None;
        }
        Some(path)
    }).flatten();

    // ── Point of no return ───────────────────────────────────────────────────

    // 4. Remove display-name symlinks pointing to this data dir (best effort).
    let display_link = paths::display_symlink(&display_name);
    let _ = fs::remove_file(&display_link);

    // 5. Atomic move to .archived/<date>-<name>/
    let archive_root = paths::archive_root();
    fs::create_dir_all(&archive_root)
        .with_context(|| format!("create archive root {archive_root:?}"))?;
    let local_archive = archive_root.join(format!("{date}-{display_name}"));
    fs::rename(&data_dir, &local_archive)
        .with_context(|| format!("archive {data_dir:?} -> {local_archive:?}"))?;

    Ok(DismissalArtifacts {
        local_archive,
        obsidian_record,
        proposals_file,
    })
}
