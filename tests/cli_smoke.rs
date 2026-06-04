use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

fn mc_bin() -> std::path::PathBuf {
    // Built by `cargo test` (debug profile). The integration-test binary lives
    // under target/.../deps/<testname>-<hash>; our binary is two levels up.
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // drop test bin filename
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("mission-control")
}

#[cfg(unix)]
fn write_fake_cmux(path: &std::path::Path, workspace_cwd: &std::path::Path, tree_succeeds: bool) {
    write_fake_cmux_with_window_refs(path, workspace_cwd, "window:1", "window:1", tree_succeeds);
}

#[cfg(unix)]
fn write_fake_cmux_with_window_refs(
    path: &std::path::Path,
    workspace_cwd: &std::path::Path,
    list_window_ref: &str,
    tree_window_ref: &str,
    tree_succeeds: bool,
) {
    let tree_body = if tree_succeeds {
        r#"cat <<'JSON'
{
  "windows": [
    {
      "ref": "window:1",
      "current": true,
      "active": true,
      "key": true,
      "workspaces": [
        {
          "ref": "workspace:1",
          "panes": [
            {
              "surfaces": [
                {
                  "ref": "surface:1",
                  "pane_ref": "pane:1",
                  "title": "shell",
                  "tty": null,
                  "selected": true,
                  "focused": true,
                  "active": true,
                  "index": 0,
                  "index_in_pane": 0,
                  "type": "terminal"
                }
              ]
            }
          ]
        }
      ]
    }
  ]
}
JSON
"#
        .replace("\"window:1\"", &format!("\"{tree_window_ref}\""))
    } else {
        "echo tree failed >&2\nexit 9\n".to_string()
    };
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  list-workspaces)
    cat <<'JSON'
{{
  "window_id": "WIN-1",
  "window_ref": "{}",
  "workspaces": [
    {{
      "ref": "workspace:1",
      "id": "WS-1",
      "title": "repo",
      "description": null,
      "current_directory": "{}",
      "custom_color": null
    }}
  ]
}}
JSON
    ;;
  tree)
    {}
    ;;
  *)
    echo "unexpected cmux args: $*" >&2
    exit 64
    ;;
esac
"#,
        list_window_ref,
        workspace_cwd.display(),
        tree_body
    );
    std::fs::write(path, script).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

#[test]
fn mc_help_shows_subcommands() {
    let output = Command::new(mc_bin())
        .arg("--help")
        .output()
        .expect("run --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("resolve"),
        "help should mention `resolve` subcommand. stdout={stdout} stderr={stderr}"
    );
    assert!(
        combined.contains("setup"),
        "help should mention `setup` subcommand. stdout={stdout} stderr={stderr}"
    );
    assert!(
        combined.contains("backfill-window"),
        "help should mention `backfill-window` subcommand. stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn mc_resolve_prints_workspace_dir() {
    let bin = mc_bin();
    let output = Command::new(&bin)
        .args(["resolve", "abc-123"])
        .output()
        .expect("run resolve");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .trim()
            .ends_with("data/mission-control/.data/abc-123"),
        "expected resolve to end with .data/abc-123, got stdout={stdout:?}"
    );
}

#[test]
fn mc_setup_creates_data_root() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(mc_bin())
        .env("HOME", tmp.path())
        .arg("setup")
        .output()
        .expect("run setup");
    assert!(
        output.status.success(),
        "setup failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root = tmp.path().join("data/mission-control");
    assert!(root.is_dir(), "expected {root:?} created");
    assert!(root.join(".data").is_dir());
    assert!(root.join(".archived").is_dir());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Created") || stdout.contains("complete"),
        "setup output should summarize changes: {stdout}"
    );
}

#[test]
fn mc_setup_creates_histories_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .arg("setup")
        .output()
        .expect("run setup");
    assert!(
        output.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let sessions = home.join("data/Sessions");
    assert!(
        sessions.is_dir(),
        "sessions dir should be created at {sessions:?}"
    );
    let histories = home.join("agents/histories");
    assert!(
        histories.is_symlink(),
        "histories path should be a symlink at {histories:?}"
    );
    assert_eq!(
        std::fs::read_link(&histories).unwrap(),
        sessions,
        "histories should point to ~/data/Sessions"
    );

    for tool in &[".claude/histories", ".codex/histories"] {
        let link = home.join(tool);
        let target = std::fs::read_link(&link).unwrap_or_else(|e| {
            panic!("expected symlink at {link:?}: {e}");
        });
        assert_eq!(target, histories, "{tool} should symlink to {histories:?}");
    }
}

#[test]
fn mc_setup_is_idempotent_on_histories_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = [("HOME", home.to_str().unwrap())];
    // First run — creates everything.
    let r1 = Command::new(mc_bin())
        .envs(env)
        .arg("setup")
        .output()
        .unwrap();
    assert!(r1.status.success());
    // Second run — must not error, must not duplicate work.
    let r2 = Command::new(mc_bin())
        .envs(env)
        .arg("setup")
        .output()
        .unwrap();
    assert!(r2.status.success());
    // Symlinks still correct.
    for tool in &[".claude/histories", ".codex/histories"] {
        assert_eq!(
            std::fs::read_link(home.join(tool)).unwrap(),
            home.join("agents/histories")
        );
    }
    assert_eq!(
        std::fs::read_link(home.join("agents/histories")).unwrap(),
        home.join("data/Sessions")
    );
}

#[cfg(unix)]
#[test]
fn mc_setup_migrates_stale_histories_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let old_sessions = home.join("obs/Agents/Sessions");
    std::fs::create_dir_all(&old_sessions).unwrap();
    std::fs::create_dir_all(home.join("agents")).unwrap();
    symlink(&old_sessions, home.join("agents/histories")).unwrap();

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .arg("setup")
        .output()
        .expect("run setup");
    assert!(
        output.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        std::fs::read_link(home.join("agents/histories")).unwrap(),
        home.join("data/Sessions")
    );
    assert!(old_sessions.is_dir(), "setup should not delete old target");
}

#[cfg(unix)]
#[test]
fn mc_backfill_window_writes_registry_with_fake_cmux() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let histories = tmp.path().join("histories");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&histories).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let fake_cmux = tmp.path().join("cmux");
    write_fake_cmux(&fake_cmux, &repo, true);

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .arg("--cmux-bin")
        .arg(&fake_cmux)
        .arg("--cmux-socket")
        .arg(tmp.path().join("cmux.sock"))
        .arg("--histories-dir")
        .arg(&histories)
        .arg("backfill-window")
        .output()
        .expect("run backfill-window");
    assert!(
        output.status.success(),
        "backfill failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let registry_dir = home.join("data/mission-control/windows/WIN-1");
    assert!(registry_dir.join("window.json").is_file());
    assert!(registry_dir.join("workspaces/WS-1.json").is_file());
    assert!(registry_dir.join("surfaces/surface_1.json").is_file());
    assert!(home
        .join("data/mission-control/.data/WS-1/trajectory.md")
        .is_file());
    let window = std::fs::read_to_string(registry_dir.join("window.json")).unwrap();
    assert!(window.contains("\"window_id\": \"WIN-1\""));
    assert!(window.contains("\"histories_valid\": true"));
    assert!(window.contains("\"repo_roots\""));
}

#[cfg(unix)]
#[test]
fn mc_backfill_window_fails_when_surface_tree_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let histories = tmp.path().join("histories");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&histories).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let fake_cmux = tmp.path().join("cmux");
    write_fake_cmux(&fake_cmux, &repo, false);

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .arg("--cmux-bin")
        .arg(&fake_cmux)
        .arg("--cmux-socket")
        .arg(tmp.path().join("cmux.sock"))
        .arg("--histories-dir")
        .arg(&histories)
        .arg("backfill-window")
        .output()
        .expect("run backfill-window");
    assert!(
        !output.status.success(),
        "backfill should fail when tree fails; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !home
            .join("data/mission-control/windows/WIN-1/window.json")
            .exists(),
        "failed backfill should not claim a registry write"
    );
}

#[cfg(unix)]
#[test]
fn mc_backfill_window_fails_when_tree_window_does_not_match_list_window() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let histories = tmp.path().join("histories");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&histories).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let fake_cmux = tmp.path().join("cmux");
    write_fake_cmux_with_window_refs(&fake_cmux, &repo, "window:2", "window:1", true);

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .arg("--cmux-bin")
        .arg(&fake_cmux)
        .arg("--cmux-socket")
        .arg(tmp.path().join("cmux.sock"))
        .arg("--histories-dir")
        .arg(&histories)
        .arg("backfill-window")
        .output()
        .expect("run backfill-window");
    assert!(
        !output.status.success(),
        "backfill should fail on window mismatch; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn mc_backfill_window_fails_on_stale_default_histories_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let old_sessions = home.join("obs/Agents/Sessions");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&old_sessions).unwrap();
    std::fs::create_dir_all(home.join("agents")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    symlink(&old_sessions, home.join("agents/histories")).unwrap();
    let fake_cmux = tmp.path().join("cmux");
    write_fake_cmux(&fake_cmux, &repo, true);

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .arg("--cmux-bin")
        .arg(&fake_cmux)
        .arg("--cmux-socket")
        .arg(tmp.path().join("cmux.sock"))
        .arg("backfill-window")
        .output()
        .expect("run backfill-window");
    assert!(
        !output.status.success(),
        "backfill should fail on stale default histories link; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !home
            .join("data/mission-control/windows/WIN-1/window.json")
            .exists(),
        "failed backfill should not write a registry"
    );
}

#[cfg(unix)]
#[test]
fn mc_backfill_window_accepts_explicit_physical_sessions_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let sessions = home.join("data/Sessions");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(home.join("agents")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    symlink(&sessions, home.join("agents/histories")).unwrap();
    let fake_cmux = tmp.path().join("cmux");
    write_fake_cmux(&fake_cmux, &repo, true);

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .arg("--cmux-bin")
        .arg(&fake_cmux)
        .arg("--cmux-socket")
        .arg(tmp.path().join("cmux.sock"))
        .arg("--histories-dir")
        .arg(&sessions)
        .arg("backfill-window")
        .output()
        .expect("run backfill-window");
    assert!(
        output.status.success(),
        "backfill should accept explicit physical sessions dir; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home
        .join("data/mission-control/windows/WIN-1/window.json")
        .is_file());
}
