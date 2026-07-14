use mission_control::mc_data::paths;

#[test]
fn data_root_under_home_data_mission_control() {
    let home = dirs::home_dir().unwrap();
    assert_eq!(paths::data_root(), home.join("data/mission-control"));
}

#[test]
fn local_config_lives_in_machine_state_root() {
    assert_eq!(
        paths::local_config_path(),
        paths::data_root().join("config.json")
    );
}

#[test]
fn workspace_dir_uses_active_uuid() {
    let p = paths::workspace_dir("7f3a-uuid");
    assert!(p.ends_with("data/mission-control/active/7f3a-uuid"));
}

#[test]
fn trajectory_path_is_inside_workspace_dir() {
    let wp = paths::workspace_dir("uuid-abc");
    assert_eq!(paths::trajectory_path("uuid-abc"), wp.join("trajectory.md"));
    assert_eq!(paths::name_path("uuid-abc"), wp.join("name"));
    assert_eq!(paths::project_path("uuid-abc"), wp.join("project"));
    assert_eq!(paths::histories_dir("uuid-abc"), wp.join("histories"));
    assert_eq!(paths::inputs_dir("uuid-abc"), wp.join("inputs"));
    assert_eq!(paths::events_log("uuid-abc"), wp.join("events.jsonl"));
    assert_eq!(paths::surfaces_dir("uuid-abc"), wp.join("surfaces"));
}

#[test]
fn archive_path_uses_date_and_name() {
    let archived = paths::archive_dir("2026-05-23", "predinvest");
    assert!(archived.ends_with("data/mission-control/.archived/2026-05-23-predinvest"));
}
