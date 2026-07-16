use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn mc_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("mission-control")
}

#[test]
fn missions_command_projects_persisted_state_without_cmux() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let agents = home.join("agents");
    let obs_agents = agents.join("obsAgents");
    let active = home.join("data/mission-control/active/ws-1");
    let window = home.join("data/mission-control/windows/window-1");
    fs::create_dir_all(&obs_agents).unwrap();
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&window).unwrap();
    fs::write(agents.join("device.json"), r#"{"name":"ref"}"#).unwrap();
    fs::write(
        window.join("window.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "window_id": "window-1",
            "window_ref": "window:1",
            "updated_at": "2026-07-16T09:00:00+00:00",
            "histories_dir": home.join("agents/histories"),
            "histories_valid": true,
            "workspaces": [{
                "schema_version": 1,
                "window_id": "window-1",
                "window_ref": "window:1",
                "workspace_id": "ws-1",
                "workspace_ref": "workspace:1",
                "name": "Mission Control",
                "current_directory": null,
                "repo_roots": [],
                "surface_refs": ["surface:1"]
            }],
            "surfaces": [{
                "schema_version": 1,
                "window_id": "window-1",
                "window_ref": "window:1",
                "workspace_id": "ws-1",
                "workspace_ref": "workspace:1",
                "surface_ref": "surface:1",
                "pane_ref": "pane:1",
                "title": "Codex",
                "tty": null,
                "kind": "codex",
                "selected": true,
                "focused": true,
                "active": true,
                "index": 0,
                "index_in_pane": 0,
                "surface_type": "terminal",
                "repo_root": null,
                "repo_source": null,
                "session_path": null,
                "session_cwd": null,
                "overall_goal": "Automate Missions notes",
                "latest_ask": "keep going"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(mc_bin())
        .env("HOME", home)
        .arg("missions")
        .output()
        .expect("run missions command");

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("created:"));
    assert!(stdout.contains(&format!("{date}-ref.md")));
    let note = obs_agents
        .join("Missions/sessions")
        .join(format!("{date}-ref.md"));
    let body = fs::read_to_string(note).unwrap();
    assert!(body.contains("Workspace: Mission Control"));
    assert!(body.contains("Current ask: keep going"));
}

#[test]
fn missions_command_returns_nonzero_when_obs_agents_is_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let agents = tmp.path().join("agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("device.json"), r#"{"name":"ref"}"#).unwrap();

    let output = Command::new(mc_bin())
        .env("HOME", tmp.path())
        .arg("missions")
        .output()
        .expect("run missions command without obsAgents");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("obsAgents root is unavailable"), "{stderr}");
    assert!(stderr.contains("create the stable symlink"), "{stderr}");
    assert!(!agents.join("obsAgents").exists());
}
