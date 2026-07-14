use std::path::PathBuf;

pub fn data_root() -> PathBuf {
    dirs::home_dir()
        .expect("home dir resolvable")
        .join("data")
        .join("mission-control")
}

/// Machine-local Mission Control configuration.
///
/// This belongs beside runtime state rather than in the repository because
/// provider choice may differ across machines.
pub fn local_config_path() -> PathBuf {
    data_root().join("config.json")
}

pub fn agent_histories_dir() -> PathBuf {
    dirs::home_dir()
        .expect("home dir resolvable")
        .join("agents")
        .join("histories")
}

/// Root for OPEN workspaces' runtime state (one dir per workspace UUID).
/// Formerly `.data`; renamed to `active` so the open/closed lifecycle reads
/// directly off the directory names.
pub fn active_root() -> PathBuf {
    data_root().join("active")
}

/// Back-compat alias for the open-workspaces root.
pub fn data_subroot() -> PathBuf {
    active_root()
}

/// Root for CLOSED workspaces' state, moved here when their UUID is no longer
/// in any live cmux window. Keyed by workspace UUID, same layout as `active/`.
pub fn archived_root() -> PathBuf {
    data_root().join("archived")
}

pub fn archived_workspace_dir(uuid: &str) -> PathBuf {
    archived_root().join(uuid)
}

/// Hidden dismissal/Obsidian-publish archive (distinct from `archived/`).
pub fn archive_root() -> PathBuf {
    data_root().join(".archived")
}

pub fn windows_root() -> PathBuf {
    data_root().join("windows")
}

pub fn window_dir(window_id: &str) -> PathBuf {
    windows_root().join(safe_path_component(window_id))
}

pub fn workspace_dir(uuid: &str) -> PathBuf {
    active_root().join(uuid)
}

pub fn name_path(uuid: &str) -> PathBuf {
    workspace_dir(uuid).join("name")
}

pub fn project_path(uuid: &str) -> PathBuf {
    workspace_dir(uuid).join("project")
}

pub fn trajectory_path(uuid: &str) -> PathBuf {
    workspace_dir(uuid).join("trajectory.md")
}

pub fn histories_dir(uuid: &str) -> PathBuf {
    workspace_dir(uuid).join("histories")
}

pub fn inputs_dir(uuid: &str) -> PathBuf {
    workspace_dir(uuid).join("inputs")
}

pub fn events_log(uuid: &str) -> PathBuf {
    workspace_dir(uuid).join("events.jsonl")
}

pub fn surfaces_dir(uuid: &str) -> PathBuf {
    workspace_dir(uuid).join("surfaces")
}

pub fn display_symlink(unique_name: &str) -> PathBuf {
    data_root().join(safe_path_component(unique_name))
}

// Asserted by tests/mc_data_paths.rs against the lib target; the bin reaches
// archive paths via dismissal.rs without calling this helper directly.
#[allow(dead_code)]
pub fn archive_dir(date: &str, unique_name: &str) -> PathBuf {
    archive_root().join(format!("{date}-{unique_name}"))
}

pub fn safe_path_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() { "_".to_string() } else { out }
}
