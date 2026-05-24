use mission_control::mc_data::{paths, workspace};

#[test]
fn ensure_workspace_idempotent_across_simulated_refreshes() {
    let tmp = tempfile::tempdir().unwrap();
    let prior = std::env::var_os("HOME");
    // SAFETY: integration test; HOME mutation is process-global. We restore
    // it at the end. (--test-threads=1 keeps this safe across tests.)
    unsafe {
        std::env::set_var("HOME", tmp.path());
    }

    let result = std::panic::catch_unwind(|| {
        // First "refresh"
        workspace::ensure_workspace("uuid-A", "alpha", "alpha").unwrap();
        let mtime1 = std::fs::metadata(paths::workspace_dir("uuid-A"))
            .unwrap()
            .modified()
            .unwrap();

        // Brief pause, then second "refresh" — must not recreate or alter the dir.
        std::thread::sleep(std::time::Duration::from_millis(50));
        workspace::ensure_workspace("uuid-A", "alpha", "alpha").unwrap();
        let mtime2 = std::fs::metadata(paths::workspace_dir("uuid-A"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(mtime1, mtime2, "second refresh should not touch the dir");

        // Display symlink is still the one we created.
        let target = std::fs::read_link(paths::display_symlink("alpha")).unwrap();
        assert!(target.to_string_lossy().contains(".data/uuid-A"));
    });

    match prior {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
