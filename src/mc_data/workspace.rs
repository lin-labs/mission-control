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
/// Whether a workspace title is worth a human-readable display symlink at the
/// data root. Path-like titles (containing `/`), dotfile-ish titles (leading
/// `.`), and empty titles sanitize into top-level junk, so they're skipped.
fn is_displayable_name(name: &str) -> bool {
    let t = name.trim();
    !t.is_empty() && !t.contains('/') && !t.starts_with('.')
}

pub fn ensure_workspace(uuid: &str, unique_name: &str, project: &str) -> Result<()> {
    let wp = paths::workspace_dir(uuid);

    // Create the tree.
    fs::create_dir_all(&wp).with_context(|| format!("create workspace dir {wp:?}"))?;
    fs::create_dir_all(paths::histories_dir(uuid))?;
    fs::create_dir_all(paths::inputs_dir(uuid))?;
    fs::create_dir_all(paths::surfaces_dir(uuid))?;

    // name and project files. Only write if the content differs — this
    // keeps the parent dir mtime stable across repeated calls (idempotent).
    write_if_changed(&paths::name_path(uuid), unique_name)?;
    write_if_changed(&paths::project_path(uuid), project)?;

    // Seed trajectory.md for the detail pane if it doesn't exist yet.
    // Idempotent: the `if !exists` guard ensures user edits are never clobbered.
    let traj_path = paths::trajectory_path(uuid);
    if !traj_path.exists() {
        let skel = crate::mc_data::trajectory::TrajectoryDoc::skeleton(uuid, unique_name, project);
        skel.save_to_file(&traj_path)
            .with_context(|| format!("seed trajectory.md at {traj_path:?}"))?;
    }

    // Display symlink at the data root — a human-readable alias for `cd`.
    // Skip path-like / dotfile / empty titles: they sanitize into top-level
    // junk (`__data_mission-control_windows`, `__.cmuxterm`, `_mosh___…`) that
    // clutters the data root without being a useful workspace name.
    if !is_displayable_name(unique_name) {
        return Ok(());
    }
    // Relative target so the symlink resolves regardless of cwd.
    let link = paths::display_symlink(unique_name);
    let target: PathBuf = PathBuf::from("active").join(uuid);
    if let Ok(existing) = fs::read_link(&link) {
        if existing == target {
            return Ok(()); // already in the right state
        }
        // pointing somewhere else: drop and recreate
        let _ = fs::remove_file(&link);
    } else if link.exists() {
        // exists but isn't a symlink — leave it alone, surface a clear error.
        anyhow::bail!("{link:?} exists and is not a symlink; refusing to overwrite");
    }
    symlink(&target, &link).with_context(|| format!("symlink {link:?} -> {target:?}"))?;
    Ok(())
}

/// Migrate the legacy `.data/` open-workspaces root to `active/`. Idempotent:
/// moves each `<uuid>` dir into `active/` (skipping any that already exist
/// there), then removes the empty `.data/`. Safe no-op when `.data/` is absent.
pub fn migrate_data_to_active() {
    let legacy = paths::data_root().join(".data");
    if !legacy.is_dir() {
        return;
    }
    let active = paths::active_root();
    let _ = fs::create_dir_all(&active);
    if let Ok(entries) = fs::read_dir(&legacy) {
        for entry in entries.flatten() {
            let from = entry.path();
            let dest = active.join(entry.file_name());
            if dest.exists() {
                continue; // already migrated; leave the legacy copy for manual review
            }
            let _ = fs::rename(&from, &dest);
        }
    }
    // Remove .data only if now empty (rename failures leave it intact).
    let _ = fs::remove_dir(&legacy);
}

/// Move workspaces whose UUID is no longer live (not in any cmux window) from
/// `active/` to `archived/`. `live_uuids` MUST be the full set across all
/// windows; an empty set is treated as "unknown" and skips archival so we never
/// archive everything when the cmux query fails. Returns the count moved.
pub fn archive_closed_workspaces(live_uuids: &std::collections::HashSet<String>) -> usize {
    if live_uuids.is_empty() {
        return 0;
    }
    let active = paths::active_root();
    let archived = paths::archived_root();
    let mut moved = 0;
    let Ok(entries) = fs::read_dir(&active) else {
        return 0;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let uuid = entry.file_name().to_string_lossy().into_owned();
        if live_uuids.contains(&uuid) {
            continue;
        }
        let _ = fs::create_dir_all(&archived);
        let dest = archived.join(&uuid);
        // If an archived copy already exists, drop the newer-but-closed one's
        // path uniqueness by replacing it (keep the latest closed state).
        if dest.exists() {
            let _ = fs::remove_dir_all(&dest);
        }
        if fs::rename(entry.path(), &dest).is_ok() {
            moved += 1;
        }
    }
    moved
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
// Exercised by tests/mc_data_workspace.rs; not yet wired into the TUI.
#[allow(dead_code)]
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
///
/// Stores `contents.trim()` so the on-disk form is canonical — readers
/// (`read_display_name`, `read_project`) also `.trim()`, so we guarantee
/// round-trip equality and never get tricked into skipping a corrective
/// write because of asymmetric whitespace.
fn write_if_changed(path: &std::path::Path, contents: &str) -> Result<()> {
    let canonical = contents.trim();
    if let Ok(existing) = fs::read_to_string(path) {
        if existing.trim() == canonical {
            return Ok(());
        }
    }
    write_atomic(path, canonical)
}

#[cfg(test)]
mod tests {
    use super::is_displayable_name;

    #[test]
    fn displayable_names_filter_path_like_and_dotfiles() {
        // Real workspace titles → keep.
        assert!(is_displayable_name("[lab] elonco"));
        assert!(is_displayable_name("agents upgrade"));
        assert!(is_displayable_name("gmail-triage"));
        // Path-like / dotfile / transient → skip (these created the junk symlinks).
        assert!(!is_displayable_name("/data/mission-control/windows"));
        assert!(!is_displayable_name("mosh [blin-labs]:~/.c/fish"));
        assert!(!is_displayable_name(".cmuxterm"));
        assert!(!is_displayable_name("~/agents/histories"));
        assert!(!is_displayable_name(""));
        assert!(!is_displayable_name("   "));
    }
}
