use mission_control::mc_data::{paths, workspace};
use std::fs;

// All tests in this file work against a per-test temp data root.
// We do this by setting HOME via env var so dirs::home_dir() returns it.
// (Each test sets a unique temp HOME to avoid cross-test interference.)
// Run with --test-threads=1 to avoid concurrent HOME mutations.

fn with_tmp_home<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests run single-threaded via --test-threads=1; HOME mutation
    // is process-global, so parallelism would cause data races.
    unsafe { std::env::set_var("HOME", tmp.path()); }
    f(tmp.path());
}

#[test]
fn ensure_workspace_dir_creates_full_tree() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-1", "predinvest", "predinvest")
            .expect("create workspace");

        let wp = paths::workspace_dir("uuid-1");
        assert!(wp.is_dir(), "workspace dir exists");
        assert!(paths::histories_dir("uuid-1").is_dir());
        assert!(paths::inputs_dir("uuid-1").is_dir());
        assert!(paths::surfaces_dir("uuid-1").is_dir());

        assert_eq!(
            std::fs::read_to_string(paths::name_path("uuid-1")).unwrap().trim(),
            "predinvest"
        );
        assert_eq!(
            std::fs::read_to_string(paths::project_path("uuid-1")).unwrap().trim(),
            "predinvest"
        );

        // Display symlink at the root points into .data/<uuid>/
        let link = paths::display_symlink("predinvest");
        let target = std::fs::read_link(&link).unwrap();
        assert!(target.to_string_lossy().contains(".data/uuid-1"));
    });
}

#[test]
fn ensure_workspace_is_idempotent() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-2", "alpha", "alpha").unwrap();
        // second call should not error and should not duplicate the symlink.
        workspace::ensure_workspace("uuid-2", "alpha", "alpha").unwrap();
        assert!(paths::workspace_dir("uuid-2").is_dir());
    });
}

#[test]
fn read_display_name_reads_name_file() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-3", "predinvest", "predinvest").unwrap();
        assert_eq!(workspace::read_display_name("uuid-3").unwrap(), "predinvest");
    });
}

#[test]
fn read_project_reads_project_file_falling_back_to_name() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-4", "ws-name", "ws-name").unwrap();
        assert_eq!(workspace::read_project("uuid-4").unwrap(), "ws-name");
        // Manually write a different project file
        fs::write(paths::project_path("uuid-4"), "different-project").unwrap();
        assert_eq!(workspace::read_project("uuid-4").unwrap(), "different-project");
    });
}
