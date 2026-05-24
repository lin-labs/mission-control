use std::process::Command;

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

#[test]
fn mc_help_shows_subcommands() {
    let output = Command::new(mc_bin())
        .arg("--help")
        .output()
        .expect("run --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    assert!(combined.contains("resolve"), "help should mention `resolve` subcommand. stdout={stdout} stderr={stderr}");
    assert!(combined.contains("setup"), "help should mention `setup` subcommand. stdout={stdout} stderr={stderr}");
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
        stdout.trim().ends_with("data/mission-control/.data/abc-123"),
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
    let obs_root = tmp.path().join("obs");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&obs_root).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(mc_bin())
        .env("HOME", &home)
        .env("OBS_AGENTS", &obs_root)
        .arg("setup")
        .output()
        .expect("run setup");
    assert!(output.status.success(), "setup failed: {}", String::from_utf8_lossy(&output.stderr));

    let sessions = obs_root.join("Sessions");
    assert!(sessions.is_dir(), "Sessions dir should be created at {sessions:?}");

    for tool in &[".claude/histories", ".codex/histories"] {
        let link = home.join(tool);
        let target = std::fs::read_link(&link).unwrap_or_else(|e| {
            panic!("expected symlink at {link:?}: {e}");
        });
        assert_eq!(target, sessions, "{tool} should symlink to {sessions:?}");
    }
}

#[test]
fn mc_setup_is_idempotent_on_histories_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let obs_root = tmp.path().join("obs");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&obs_root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let env = [("HOME", home.to_str().unwrap()), ("OBS_AGENTS", obs_root.to_str().unwrap())];
    // First run — creates everything.
    let r1 = Command::new(mc_bin()).envs(env).arg("setup").output().unwrap();
    assert!(r1.status.success());
    // Second run — must not error, must not duplicate work.
    let r2 = Command::new(mc_bin()).envs(env).arg("setup").output().unwrap();
    assert!(r2.status.success());
    // Symlinks still correct.
    for tool in &[".claude/histories", ".codex/histories"] {
        assert_eq!(std::fs::read_link(home.join(tool)).unwrap(), obs_root.join("Sessions"));
    }
}
