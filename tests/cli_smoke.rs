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
