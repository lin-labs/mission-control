//! Daily, device-scoped Obsidian projection of Mission Control state.
//!
//! Mission Control owns the generated region only. Human-authored text before
//! or after the markers is preserved byte-for-byte on every refresh.

use crate::mc_data::window_registry::{SurfaceRegistration, WindowRegistry, WorkspaceRegistration};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const START_MARKER: &str = "<!-- mission-control:generated:start -->";
pub const END_MARKER: &str = "<!-- mission-control:generated:end -->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    Created(PathBuf),
    Updated(PathBuf),
    Unchanged(PathBuf),
}

#[derive(Debug, Deserialize)]
struct DeviceMetadata {
    name: String,
}

#[derive(Debug)]
struct CurrentWorkspace {
    observed_at: String,
    workspace: WorkspaceRegistration,
    surfaces: Vec<SurfaceRegistration>,
}

/// Refresh today's Missions note using the standard machine-local paths.
pub fn sync_default() -> Result<SyncOutcome> {
    let home = dirs::home_dir().context("resolve home directory for Obsidian Missions sync")?;
    let agents = home.join("agents");
    let device_json = agents.join("device.json");
    let legacy_device = agents.join(".device");
    let device_file = if device_json.is_file() {
        device_json
    } else {
        legacy_device
    };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    sync_from_paths(
        &crate::mc_data::paths::windows_root(),
        &crate::mc_data::paths::active_root(),
        &agents.join("obsAgents"),
        &device_file,
        &date,
    )
}

/// Refresh one daily Missions note from persisted window registries.
///
/// `windows_root` may contain stale windows. Only workspace IDs still present
/// under `active_root` are eligible, and the newest registry wins when a
/// workspace appears in more than one window snapshot.
pub fn sync_from_paths(
    windows_root: &Path,
    active_root: &Path,
    obs_agents_root: &Path,
    device_file: &Path,
    date: &str,
) -> Result<SyncOutcome> {
    if !obs_agents_root.is_dir() {
        bail!(
            "obsAgents root is unavailable at {}; create the stable symlink before enabling Missions sync",
            obs_agents_root.display()
        );
    }
    validate_filename_component(date, "PT date")?;
    let device = read_device_name(device_file)?;
    validate_filename_component(&device, "device name")?;

    let workspaces = load_current_workspaces(windows_root, active_root)?;
    if workspaces.is_empty() {
        bail!(
            "no current workspace snapshots found under {} for active workspaces in {}",
            windows_root.display(),
            active_root.display()
        );
    }

    let managed = render_managed_region(workspaces);
    let sessions_dir = obs_agents_root.join("Missions").join("sessions");
    let note_path = sessions_dir.join(format!("{date}-{device}.md"));

    if note_path.is_file() {
        let existing = std::fs::read_to_string(&note_path)
            .with_context(|| format!("read Missions note {}", note_path.display()))?;
        let updated = merge_managed_region(&existing, &managed)?;
        if updated == existing {
            return Ok(SyncOutcome::Unchanged(note_path));
        }
        atomic_write(&note_path, &updated)?;
        return Ok(SyncOutcome::Updated(note_path));
    }

    std::fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("create Missions sessions dir {}", sessions_dir.display()))?;
    let note = format!(
        "---\ndate: {date}\ndevice: {device}\nsource: mission-control\n---\n\n# Missions — {} — {date}\n\n{managed}\n",
        markdown_inline(&device)
    );
    atomic_write(&note_path, &note)?;
    Ok(SyncOutcome::Created(note_path))
}

fn read_device_name(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read device metadata {}", path.display()))?;
    let trimmed = raw.trim();
    let name = if trimmed.starts_with('{') {
        serde_json::from_str::<DeviceMetadata>(trimmed)
            .with_context(|| format!("parse device metadata {}", path.display()))?
            .name
    } else {
        trimmed.to_string()
    };
    if name.trim().is_empty() {
        bail!("device metadata {} has an empty name", path.display());
    }
    Ok(name.trim().to_string())
}

fn validate_filename_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("{label} is not a safe filename component: {value:?}");
    }
    Ok(())
}

fn load_current_workspaces(
    windows_root: &Path,
    active_root: &Path,
) -> Result<Vec<CurrentWorkspace>> {
    let active_ids: HashSet<String> = std::fs::read_dir(active_root)
        .with_context(|| format!("read active workspaces {}", active_root.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .and_then(|_| entry.file_name().into_string().ok())
        })
        .collect();

    let mut registry_paths: Vec<PathBuf> = std::fs::read_dir(windows_root)
        .with_context(|| format!("read window snapshots {}", windows_root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("window.json"))
        .filter(|path| path.is_file())
        .collect();
    registry_paths.sort();

    let mut newest: HashMap<String, CurrentWorkspace> = HashMap::new();
    for path in registry_paths {
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let registry: WindowRegistry = match serde_json::from_str(&raw) {
            Ok(registry) => registry,
            Err(_) => continue,
        };
        let mut surfaces_by_workspace: HashMap<String, Vec<SurfaceRegistration>> = HashMap::new();
        for surface in registry.surfaces {
            if active_ids.contains(&surface.workspace_id) {
                surfaces_by_workspace
                    .entry(surface.workspace_id.clone())
                    .or_default()
                    .push(surface);
            }
        }
        for workspace in registry.workspaces {
            if !active_ids.contains(&workspace.workspace_id) {
                continue;
            }
            let candidate = CurrentWorkspace {
                observed_at: registry.updated_at.clone(),
                surfaces: surfaces_by_workspace
                    .remove(&workspace.workspace_id)
                    .unwrap_or_default(),
                workspace,
            };
            let replace = newest
                .get(&candidate.workspace.workspace_id)
                .is_none_or(|current| candidate.observed_at > current.observed_at);
            if replace {
                newest.insert(candidate.workspace.workspace_id.clone(), candidate);
            }
        }
    }

    let mut current: Vec<CurrentWorkspace> = newest.into_values().collect();
    current.sort_by(|a, b| {
        a.workspace
            .name
            .to_lowercase()
            .cmp(&b.workspace.name.to_lowercase())
            .then_with(|| a.workspace.workspace_id.cmp(&b.workspace.workspace_id))
    });
    for workspace in &mut current {
        workspace.surfaces.sort_by(|a, b| {
            a.index
                .unwrap_or(u32::MAX)
                .cmp(&b.index.unwrap_or(u32::MAX))
                .then_with(|| a.surface_ref.cmp(&b.surface_ref))
        });
    }
    Ok(current)
}

fn render_managed_region(workspaces: Vec<CurrentWorkspace>) -> String {
    let mut out = String::from(START_MARKER);
    out.push_str(
        "\n- Overall goal: Maintain a daily, device-scoped map of each cmux workspace and surface.\n",
    );
    for current in workspaces {
        out.push_str("- Workspace: ");
        out.push_str(&markdown_inline(&current.workspace.name));
        out.push('\n');
        for surface in current.surfaces {
            let title = if surface.title.trim().is_empty() {
                "(untitled)"
            } else {
                surface.title.trim()
            };
            out.push_str("  - Surface: ");
            out.push_str(&markdown_inline(title));
            out.push_str(" (`");
            out.push_str(&surface.surface_ref);
            out.push_str("`, ");
            out.push_str(surface.kind.label());
            out.push_str(")\n");
            if let Some(goal) = surface
                .overall_goal
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                out.push_str("    - Overall goal: ");
                out.push_str(&markdown_inline(goal));
                out.push('\n');
            }
            if let Some(ask) = surface
                .latest_ask
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                out.push_str("    - Current ask: ");
                out.push_str(&markdown_inline(ask));
                out.push('\n');
            }
        }
    }
    out.push_str(END_MARKER);
    out
}

fn markdown_inline(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut escaped = String::with_capacity(collapsed.len());
    for ch in collapsed.chars() {
        if matches!(ch, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn merge_managed_region(existing: &str, managed: &str) -> Result<String> {
    match (existing.find(START_MARKER), existing.find(END_MARKER)) {
        (Some(start), Some(end)) if end >= start => {
            let suffix_start = end + END_MARKER.len();
            let mut out = String::with_capacity(existing.len() + managed.len());
            out.push_str(&existing[..start]);
            out.push_str(managed);
            out.push_str(&existing[suffix_start..]);
            Ok(out)
        }
        (None, None) => {
            let mut out = existing.to_string();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str(managed);
            out.push('\n');
            Ok(out)
        }
        _ => bail!(
            "Missions note contains an unmatched mission-control generated marker; refusing to overwrite human content"
        ),
    }
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("Missions note has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create Missions note parent {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("missions.md");
    let tmp = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, contents).with_context(|| format!("write {}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
        }
    }
}
