use std::path::PathBuf;

pub fn data_root() -> PathBuf {
    dirs::home_dir()
        .expect("home dir resolvable")
        .join("data")
        .join("mission-control")
}

pub fn data_subroot() -> PathBuf {
    data_root().join(".data")
}

pub fn archive_root() -> PathBuf {
    data_root().join(".archived")
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
    data_root().join(unique_name)
}

pub fn archive_dir(date: &str, unique_name: &str) -> PathBuf {
    archive_root().join(format!("{date}-{unique_name}"))
}
