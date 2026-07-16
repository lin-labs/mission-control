use mission_control::mc_data::missions::{END_MARKER, START_MARKER, SyncOutcome, sync_from_paths};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

fn workspace(id: &str, name: &str, surface_refs: &[&str]) -> Value {
    json!({
        "schema_version": 1,
        "window_id": "window-1",
        "window_ref": "window:1",
        "workspace_id": id,
        "workspace_ref": "workspace:1",
        "name": name,
        "current_directory": null,
        "repo_roots": [],
        "surface_refs": surface_refs,
    })
}

fn surface(
    workspace_id: &str,
    surface_ref: &str,
    index: u32,
    title: &str,
    kind: &str,
    overall_goal: Option<&str>,
    latest_ask: Option<&str>,
) -> Value {
    json!({
        "schema_version": 1,
        "window_id": "window-1",
        "window_ref": "window:1",
        "workspace_id": workspace_id,
        "workspace_ref": "workspace:1",
        "surface_ref": surface_ref,
        "pane_ref": "pane:1",
        "title": title,
        "tty": null,
        "kind": kind,
        "selected": false,
        "focused": false,
        "active": false,
        "index": index,
        "index_in_pane": index,
        "surface_type": "terminal",
        "repo_root": null,
        "repo_source": null,
        "session_path": null,
        "session_cwd": null,
        "overall_goal": overall_goal,
        "latest_ask": latest_ask,
    })
}

fn write_registry(
    windows_root: &Path,
    dir_name: &str,
    updated_at: &str,
    workspaces: Vec<Value>,
    surfaces: Vec<Value>,
) {
    let dir = windows_root.join(dir_name);
    fs::create_dir_all(&dir).unwrap();
    let registry = json!({
        "schema_version": 1,
        "window_id": dir_name,
        "window_ref": "window:1",
        "updated_at": updated_at,
        "histories_dir": "/tmp/histories",
        "histories_valid": true,
        "workspaces": workspaces,
        "surfaces": surfaces,
    });
    fs::write(
        dir.join("window.json"),
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .unwrap();
}

fn fixture_roots(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let windows = tmp.path().join("windows");
    let active = tmp.path().join("active");
    let obs_agents = tmp.path().join("obsAgents");
    let device = tmp.path().join("device.json");
    fs::create_dir_all(&windows).unwrap();
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&obs_agents).unwrap();
    fs::write(&device, r#"{"name":"ref"}"#).unwrap();
    (windows, active, obs_agents, device)
}

fn note_path(obs_agents: &Path) -> PathBuf {
    obs_agents
        .join("Missions/sessions")
        .join("2026-07-16-ref.md")
}

#[test]
fn creates_daily_note_from_current_workspaces_in_deterministic_order() {
    let tmp = tempfile::tempdir().unwrap();
    let (windows, active, obs_agents, device) = fixture_roots(&tmp);
    fs::create_dir_all(active.join("ws-alpha")).unwrap();
    fs::create_dir_all(active.join("ws-beta")).unwrap();

    write_registry(
        &windows,
        "old-window",
        "2026-07-15T09:00:00+00:00",
        vec![workspace("ws-beta", "Beta", &["surface:9"])],
        vec![surface(
            "ws-beta",
            "surface:9",
            0,
            "stale title",
            "codex",
            Some("stale goal"),
            None,
        )],
    );
    write_registry(
        &windows,
        "current-window",
        "2026-07-16T09:00:00+00:00",
        vec![
            workspace("ws-beta", "Beta", &["surface:2", "surface:1"]),
            workspace("ws-alpha", "Alpha", &["surface:3"]),
            workspace("ws-closed", "Closed", &["surface:4"]),
        ],
        vec![
            surface("ws-beta", "surface:2", 1, "Second", "remote", None, None),
            surface(
                "ws-beta",
                "surface:1",
                0,
                "First",
                "codex",
                Some("Ship\nissue #123"),
                Some("Preserve [notes]"),
            ),
            surface(
                "ws-alpha",
                "surface:3",
                0,
                "Alpha agent",
                "claude",
                Some("Alpha goal"),
                None,
            ),
            surface(
                "ws-closed",
                "surface:4",
                0,
                "Closed agent",
                "claude",
                Some("must not render"),
                None,
            ),
        ],
    );

    let outcome = sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16").unwrap();

    assert_eq!(outcome, SyncOutcome::Created(note_path(&obs_agents)));
    let note = fs::read_to_string(note_path(&obs_agents)).unwrap();
    assert!(note.contains(START_MARKER));
    assert!(note.contains(END_MARKER));
    assert!(note.contains("date: 2026-07-16"));
    assert!(note.contains("device: ref"));
    assert!(!note.contains("Closed"));
    assert!(!note.contains("stale title"));
    assert!(note.contains("Ship issue \\#123"));
    assert!(note.contains("Preserve \\[notes\\]"));
    let alpha = note.find("Workspace: Alpha").unwrap();
    let beta = note.find("Workspace: Beta").unwrap();
    let first = note.find("Surface: First").unwrap();
    let second = note.find("Surface: Second").unwrap();
    assert!(alpha < beta, "workspaces must sort by name");
    assert!(first < second, "surfaces must sort by index");
}

#[test]
fn appends_managed_region_to_markerless_note_without_changing_human_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (windows, active, obs_agents, device) = fixture_roots(&tmp);
    fs::create_dir_all(active.join("ws-1")).unwrap();
    write_registry(
        &windows,
        "window-1",
        "2026-07-16T09:00:00+00:00",
        vec![workspace("ws-1", "Manual", &["surface:1"])],
        vec![surface(
            "ws-1",
            "surface:1",
            0,
            "Agent",
            "codex",
            Some("Generated goal"),
            None,
        )],
    );
    let path = note_path(&obs_agents);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let human = "# My missions\n\n- Human note: never overwrite me.\n";
    fs::write(&path, human).unwrap();

    let outcome = sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16").unwrap();

    assert_eq!(outcome, SyncOutcome::Updated(path.clone()));
    let note = fs::read_to_string(path).unwrap();
    assert!(note.starts_with(human));
    assert!(note.contains("Generated goal"));
    assert_eq!(note.matches(START_MARKER).count(), 1);
}

#[test]
fn replaces_only_managed_region_and_preserves_surrounding_human_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (windows, active, obs_agents, device) = fixture_roots(&tmp);
    fs::create_dir_all(active.join("ws-1")).unwrap();
    write_registry(
        &windows,
        "window-1",
        "2026-07-16T09:00:00+00:00",
        vec![workspace("ws-1", "Live", &["surface:1"])],
        vec![surface(
            "ws-1",
            "surface:1",
            0,
            "Agent",
            "codex",
            Some("New goal"),
            None,
        )],
    );
    let path = note_path(&obs_agents);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        format!(
            "Human before\n\n{START_MARKER}\n- Old generated row\n{END_MARKER}\n\nHuman after\n"
        ),
    )
    .unwrap();

    let outcome = sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16").unwrap();

    assert_eq!(outcome, SyncOutcome::Updated(path.clone()));
    let note = fs::read_to_string(path).unwrap();
    assert!(note.starts_with("Human before"));
    assert!(note.ends_with("Human after\n"));
    assert!(note.contains("New goal"));
    assert!(!note.contains("Old generated row"));
}

#[test]
fn unchanged_generated_content_does_not_rewrite_note() {
    let tmp = tempfile::tempdir().unwrap();
    let (windows, active, obs_agents, device) = fixture_roots(&tmp);
    fs::create_dir_all(active.join("ws-1")).unwrap();
    write_registry(
        &windows,
        "window-1",
        "2026-07-16T09:00:00+00:00",
        vec![workspace("ws-1", "Stable", &["surface:1"])],
        vec![surface(
            "ws-1",
            "surface:1",
            0,
            "Agent",
            "codex",
            Some("Same goal"),
            None,
        )],
    );
    sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16").unwrap();

    let outcome = sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16").unwrap();

    assert_eq!(outcome, SyncOutcome::Unchanged(note_path(&obs_agents)));
}

#[test]
fn missing_obs_agents_root_fails_without_creating_fallback_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let (windows, active, obs_agents, device) = fixture_roots(&tmp);
    fs::remove_dir(&obs_agents).unwrap();

    let error = sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16")
        .unwrap_err()
        .to_string();

    assert!(error.contains("obsAgents"));
    assert!(error.contains("create the stable symlink"));
    assert!(!obs_agents.exists());
}

#[test]
fn missing_device_metadata_is_actionable_and_non_destructive() {
    let tmp = tempfile::tempdir().unwrap();
    let (windows, active, obs_agents, device) = fixture_roots(&tmp);
    fs::remove_file(&device).unwrap();

    let error = sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16")
        .unwrap_err()
        .to_string();

    assert!(error.contains("device metadata"));
    assert!(!obs_agents.join("Missions").exists());
}

#[test]
fn no_current_snapshot_data_fails_without_creating_a_note() {
    let tmp = tempfile::tempdir().unwrap();
    let (windows, active, obs_agents, device) = fixture_roots(&tmp);
    fs::create_dir_all(active.join("ws-without-window")).unwrap();

    let error = sync_from_paths(&windows, &active, &obs_agents, &device, "2026-07-16")
        .unwrap_err()
        .to_string();

    assert!(error.contains("no current workspace snapshots"));
    assert!(!note_path(&obs_agents).exists());
}
