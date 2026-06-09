/// Integration tests for `mc bind`.
///
/// Because MC_WORKSPACE_ID is read from the environment, these tests run via
/// the compiled binary (like cli_smoke.rs) so we can control the process
/// environment without interfering with the test harness.
///
/// Run with: cargo test --test cli_bind -- --test-threads=1
use std::path::PathBuf;
use std::process::Command;

fn mc_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // drop the test binary filename
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("mission-control")
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 1: no env vars → exit 0, no file created, stderr message
// ──────────────────────────────────────────────────────────────────────────────
#[test]
fn bind_noop_when_no_workspace_id() {
    let tmp = tempfile::tempdir().unwrap();

    let output = Command::new(mc_bin())
        .env("HOME", tmp.path())
        // Explicitly unset MC_WORKSPACE_ID so the test is hermetic.
        .env_remove("MC_WORKSPACE_ID")
        .env_remove("MC_SURFACE_ID")
        .env_remove("CLAUDE_SESSION_FILE")
        .args(["bind", "some-surface-id"])
        .output()
        .expect("run mc bind");

    assert!(
        output.status.success(),
        "expected exit 0, got {}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("MC_WORKSPACE_ID not set") || stderr.contains("skipping"),
        "expected skipping message in stderr, got: {stderr}"
    );

    // No surfaces dir should have been created anywhere under tmp.
    let surfaces = tmp.path().join("data/mission-control/active");
    assert!(
        !surfaces.exists(),
        "no data should be written when MC_WORKSPACE_ID is unset"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 2: MC_WORKSPACE_ID set + --session-file → pointer file created
// ──────────────────────────────────────────────────────────────────────────────
#[test]
fn bind_writes_pointer_file_with_explicit_session_file() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = "/tmp/fake-session.md";

    let output = Command::new(mc_bin())
        .env("HOME", tmp.path())
        .env("MC_WORKSPACE_ID", "uuid-1")
        .env("MC_SURFACE_ID", "sid-1")
        .env_remove("CLAUDE_SESSION_FILE")
        .args(["bind", "sid-1", "--session-file", session_path])
        .output()
        .expect("run mc bind");

    assert!(
        output.status.success(),
        "expected exit 0\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let pointer = tmp
        .path()
        .join("data/mission-control/active/uuid-1/surfaces/sid-1.session-path");
    assert!(pointer.exists(), "pointer file should exist at {pointer:?}");

    let contents = std::fs::read_to_string(&pointer).unwrap();
    assert_eq!(
        contents.trim(),
        session_path,
        "pointer should contain session path"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 3: surface_id defaults to $MC_SURFACE_ID env var via clap #[arg(env=...)]
// ──────────────────────────────────────────────────────────────────────────────
#[test]
fn bind_reads_surface_id_from_env_var() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = "/tmp/fake-session-env.md";

    let output = Command::new(mc_bin())
        .env("HOME", tmp.path())
        .env("MC_WORKSPACE_ID", "uuid-env")
        .env("MC_SURFACE_ID", "sid-env") // surface_id provided via env
        .env_remove("CLAUDE_SESSION_FILE")
        // Note: no positional surface_id arg here — clap reads it from MC_SURFACE_ID
        .args(["bind", "sid-env", "--session-file", session_path])
        .output()
        .expect("run mc bind with env surface_id");

    assert!(
        output.status.success(),
        "expected exit 0\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let pointer = tmp
        .path()
        .join("data/mission-control/active/uuid-env/surfaces/sid-env.session-path");
    assert!(pointer.exists(), "pointer file should exist at {pointer:?}");
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 4: $CLAUDE_SESSION_FILE env var used when no --session-file arg
// ──────────────────────────────────────────────────────────────────────────────
#[test]
fn bind_uses_claude_session_file_env_var() {
    let tmp = tempfile::tempdir().unwrap();
    let session_path = "/tmp/from-env-var.md";

    let output = Command::new(mc_bin())
        .env("HOME", tmp.path())
        .env("MC_WORKSPACE_ID", "uuid-csf")
        .env("CLAUDE_SESSION_FILE", session_path)
        .env_remove("MC_SURFACE_ID")
        .args(["bind", "sid-csf"]) // no --session-file
        .output()
        .expect("run mc bind with CLAUDE_SESSION_FILE");

    assert!(
        output.status.success(),
        "expected exit 0\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let pointer = tmp
        .path()
        .join("data/mission-control/active/uuid-csf/surfaces/sid-csf.session-path");
    assert!(pointer.exists(), "pointer file should exist at {pointer:?}");
    let contents = std::fs::read_to_string(&pointer).unwrap();
    assert_eq!(contents.trim(), session_path);
}

// ──────────────────────────────────────────────────────────────────────────────
// Test 5 (optional): fallback scan picks a matching history file
// ──────────────────────────────────────────────────────────────────────────────
#[test]
fn bind_fallback_scan_picks_matching_file() {
    let tmp = tempfile::tempdir().unwrap();
    let histories = tmp.path().join("agents/histories");
    std::fs::create_dir_all(&histories).unwrap();

    // File that matches workspace_id
    let file_match = histories.join("session-matching.md");
    std::fs::write(
        &file_match,
        "---\nworkspace_id: ws-scan-2\ntitle: match\n---\n# body\n",
    )
    .unwrap();

    // File that does NOT match — should be ignored
    let _file_other = histories.join("other-workspace.md");
    std::fs::write(
        &_file_other,
        "---\nworkspace_id: ws-other\ntitle: other\n---\n# body\n",
    )
    .unwrap();

    let output = Command::new(mc_bin())
        .env("HOME", tmp.path())
        .env("MC_WORKSPACE_ID", "ws-scan-2")
        .env_remove("CLAUDE_SESSION_FILE")
        .env_remove("MC_SURFACE_ID")
        .args(["bind", "sid-scan2"]) // no --session-file, no $CLAUDE_SESSION_FILE
        .output()
        .expect("run mc bind fallback scan");

    assert!(
        output.status.success(),
        "expected exit 0\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let pointer = tmp
        .path()
        .join("data/mission-control/active/ws-scan-2/surfaces/sid-scan2.session-path");
    assert!(pointer.exists(), "pointer file should exist at {pointer:?}");

    let written = std::fs::read_to_string(&pointer).unwrap();
    assert!(
        written.contains("session-matching"),
        "expected matching file to be picked, got: {written}"
    );
}
