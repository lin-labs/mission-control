use mission_control::mc_data::{paths, workspace};
use std::fs;
use std::os::unix::fs::MetadataExt;

// All tests in this file work against a per-test temp data root.
// We do this by setting HOME via env var so dirs::home_dir() returns it.
// (Each test sets a unique temp HOME to avoid cross-test interference.)
// Run with --test-threads=1 to avoid concurrent HOME mutations.

fn with_tmp_home<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("HOME");
    // SAFETY: tests run single-threaded via --test-threads=1; HOME mutation
    // is process-global, so parallelism would cause data races.
    unsafe { std::env::set_var("HOME", tmp.path()) };
    // Restore HOME even if the closure panics — otherwise a failing test
    // leaves later tests pointing at the already-dropped tempdir.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp.path())));
    match prior {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
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
            std::fs::read_to_string(paths::name_path("uuid-1"))
                .unwrap()
                .trim(),
            "predinvest"
        );
        assert_eq!(
            std::fs::read_to_string(paths::project_path("uuid-1"))
                .unwrap()
                .trim(),
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
        assert_eq!(
            workspace::read_display_name("uuid-3").unwrap(),
            "predinvest"
        );
    });
}

#[test]
fn read_project_reads_project_file() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-4", "ws-name", "ws-name").unwrap();
        assert_eq!(workspace::read_project("uuid-4").unwrap(), "ws-name");
        // Overwriting the project file is reflected on subsequent reads.
        fs::write(paths::project_path("uuid-4"), "different-project").unwrap();
        assert_eq!(
            workspace::read_project("uuid-4").unwrap(),
            "different-project"
        );
    });
}

#[test]
fn rename_workspace_moves_only_the_symlink() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-r1", "predinvest", "predinvest").unwrap();
        let data_path = paths::workspace_dir("uuid-r1");
        let inode_before = fs::metadata(&data_path).unwrap().ino();

        workspace::rename_workspace("uuid-r1", "predinvest-v2").unwrap();

        // The data dir is untouched.
        assert_eq!(inode_before, fs::metadata(&data_path).unwrap().ino());
        // The old symlink is gone.
        assert!(!paths::display_symlink("predinvest").exists());
        // The new symlink exists and resolves.
        let new_link = paths::display_symlink("predinvest-v2");
        assert!(new_link.exists());
        let resolved = fs::canonicalize(&new_link).unwrap();
        assert_eq!(resolved, fs::canonicalize(&data_path).unwrap());
        // The name file reflects the new name.
        assert_eq!(
            workspace::read_display_name("uuid-r1").unwrap(),
            "predinvest-v2"
        );
    });
}

#[test]
fn rename_workspace_to_same_name_is_noop() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-r2", "alpha", "alpha").unwrap();
        workspace::rename_workspace("uuid-r2", "alpha").unwrap(); // should succeed silently
        assert!(paths::display_symlink("alpha").exists());
        assert_eq!(workspace::read_display_name("uuid-r2").unwrap(), "alpha");
    });
}

#[test]
fn ensure_workspace_creates_skeleton_trajectory_on_first_run() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-b1", "alpha", "alpha").unwrap();
        let traj_path = paths::trajectory_path("uuid-b1");
        assert!(traj_path.exists(), "trajectory.md should be auto-created");
        let content = std::fs::read_to_string(&traj_path).unwrap();
        assert!(content.contains("## Mission"));
        assert!(content.contains("## Current surfaces"));
        assert!(content.contains("## Beads"));
    });
}

#[test]
fn ensure_workspace_handles_names_with_slashes() {
    with_tmp_home(|home| {
        workspace::ensure_workspace("uuid-slash", "~/Boyan", "~/Boyan").unwrap();
        assert!(paths::workspace_dir("uuid-slash").is_dir());
        assert_eq!(
            std::fs::read_link(home.join("data/mission-control/__Boyan")).unwrap(),
            std::path::PathBuf::from(".data").join("uuid-slash")
        );
    });
}

#[test]
fn ensure_workspace_does_not_overwrite_existing_trajectory() {
    with_tmp_home(|_| {
        workspace::ensure_workspace("uuid-b2", "beta", "beta").unwrap();
        let traj_path = paths::trajectory_path("uuid-b2");
        // User hand-edits the trajectory.
        std::fs::write(&traj_path, "## Mission\n- my custom goal\n").unwrap();
        // Refresh fires ensure_workspace again — must NOT clobber.
        workspace::ensure_workspace("uuid-b2", "beta", "beta").unwrap();
        let content = std::fs::read_to_string(&traj_path).unwrap();
        assert!(
            content.contains("my custom goal"),
            "must preserve user edits"
        );
    });
}
