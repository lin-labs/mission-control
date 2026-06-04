use std::path::PathBuf;

pub fn data_root() -> PathBuf {
    dirs::home_dir()
        .expect("home dir resolvable")
        .join("data")
        .join("mission-control")
}

pub fn agent_histories_dir() -> PathBuf {
    dirs::home_dir()
        .expect("home dir resolvable")
        .join("agents")
        .join("histories")
}

pub fn session_logs_dir() -> PathBuf {
    dirs::home_dir()
        .expect("home dir resolvable")
        .join("data")
        .join("Sessions")
}

pub fn data_subroot() -> PathBuf {
    data_root().join(".data")
}

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
    data_subroot().join(uuid)
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
