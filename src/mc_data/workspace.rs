use crate::mc_data::paths;
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

/// Create the workspace data dir + display symlink if missing. Idempotent.
///
/// `uuid` is the cmux workspace UUID (stable forever).
/// `unique_name` is the current display name from cmux.
/// `project` is the project name (defaults to `unique_name`).
pub fn ensure_workspace(uuid: &str, unique_name: &str, project: &str) -> Result<()> {
    let wp = paths::workspace_dir(uuid);

    // Create the tree.
    fs::create_dir_all(&wp)
        .with_context(|| format!("create workspace dir {wp:?}"))?;
    fs::create_dir_all(paths::histories_dir(uuid))?;
    fs::create_dir_all(paths::inputs_dir(uuid))?;
    fs::create_dir_all(paths::surfaces_dir(uuid))?;

    // name and project files. Only write if the content differs — this
    // keeps the parent dir mtime stable across repeated calls (idempotent).
    write_if_changed(&paths::name_path(uuid), unique_name)?;
    write_if_changed(&paths::project_path(uuid), project)?;

    // Display symlink at the data root. Relative target so the symlink
    // resolves correctly inside the data dir regardless of cwd.
    let link = paths::display_symlink(unique_name);
    let target: PathBuf = PathBuf::from(".data").join(uuid);
    if let Ok(existing) = fs::read_link(&link) {
        if existing == target {
            return Ok(()); // already in the right state
        }
        // pointing somewhere else: drop and recreate
        let _ = fs::remove_file(&link);
    } else if link.exists() {
        // exists but isn't a symlink — leave it alone, surface a clear error.
        anyhow::bail!(
            "{link:?} exists and is not a symlink; refusing to overwrite"
        );
    }
    symlink(&target, &link)
        .with_context(|| format!("symlink {link:?} -> {target:?}"))?;
    Ok(())
}

pub fn read_display_name(uuid: &str) -> Result<String> {
    Ok(fs::read_to_string(paths::name_path(uuid))?
        .trim()
        .to_string())
}

pub fn read_project(uuid: &str) -> Result<String> {
    Ok(fs::read_to_string(paths::project_path(uuid))?
        .trim()
        .to_string())
}

/// Rename a workspace's display name. Moves the symlink only; the data dir
/// (keyed by UUID) does not move. Rewrites the `name` file.
pub fn rename_workspace(uuid: &str, new_name: &str) -> Result<()> {
    let old_name = read_display_name(uuid)?;
    if old_name == new_name {
        return Ok(());
    }
    let old_link = paths::display_symlink(&old_name);
    let new_link = paths::display_symlink(new_name);
    // mv on the symlink only. Atomic via rename(2).
    fs::rename(&old_link, &new_link)
        .with_context(|| format!("rename symlink {old_link:?} -> {new_link:?}"))?;
    write_atomic(&paths::name_path(uuid), new_name)?;
    Ok(())
}

fn write_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Write only if the file is absent or its contents differ. This avoids
/// touching the parent directory's mtime on repeated no-op calls.
fn write_if_changed(path: &std::path::Path, contents: &str) -> Result<()> {
    if let Ok(existing) = fs::read_to_string(path) {
        if existing.trim() == contents.trim() {
            return Ok(());
        }
    }
    write_atomic(path, contents)
}
