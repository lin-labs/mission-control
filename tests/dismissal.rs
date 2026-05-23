/// Integration tests for the workspace dismissal flow.
///
/// These tests do NOT call a real LLM. They exercise `dismissal::finalize`
/// (pure file ops) and the App dismissal methods.
///
/// Run with `--test-threads=1` because HOME and OBS_AGENTS are process-global.
use anyhow::Result;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a closure with HOME and OBS_AGENTS both set to a fresh temp directory.
/// Both are restored on exit (even on panic).
fn with_tmp_home_and_obs<F: FnOnce(&Path) -> Result<()>>(f: F) -> Result<()> {
    let tmp = TempDir::new()?;
    let obs_dir = tmp.path().join("obsagents");
    fs::create_dir_all(&obs_dir)?;

    let prior_home = std::env::var_os("HOME");
    let prior_obs = std::env::var_os("OBS_AGENTS");

    // SAFETY: tests must run with --test-threads=1; env vars are process-global.
    unsafe {
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("OBS_AGENTS", &obs_dir);
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&obs_dir)));

    unsafe {
        match prior_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prior_obs {
            Some(v) => std::env::set_var("OBS_AGENTS", v),
            None => std::env::remove_var("OBS_AGENTS"),
        }
    }

    match result {
        Ok(r) => r,
        Err(e) => std::panic::resume_unwind(e),
    }
}

/// Create a minimal workspace data dir that `finalize` can work with.
/// The HOME env var must already be set to `home_dir` before calling.
/// Returns the uuid used.
fn create_test_workspace(name: &str) -> Result<String> {
    let uuid = format!("test-uuid-{name}");
    // Use the paths module — it reads HOME via dirs::home_dir().
    mission_control::mc_data::workspace::ensure_workspace(&uuid, name, name)?;
    Ok(uuid)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn finalize_produces_local_archive() -> Result<()> {
    with_tmp_home_and_obs(|_obs| {
        let uuid = create_test_workspace("my-ws")?;

        let artifacts = mission_control::mc_data::dismissal::finalize(
            &uuid,
            "# Learning\n\nSome content.\n",
            None,
        )?;

        assert!(
            artifacts.local_archive.exists(),
            "local archive dir should exist at {:?}",
            artifacts.local_archive
        );
        // Archive should be under .archived/ (use paths module for the root)
        let archive_root = mission_control::mc_data::paths::archive_root();
        assert!(
            artifacts.local_archive.starts_with(&archive_root),
            "archive should be under .archived/"
        );
        // Name should contain the workspace name
        let archive_name = artifacts
            .local_archive
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            archive_name.contains("my-ws"),
            "archive name should contain workspace name, got {archive_name}"
        );
        Ok(())
    })
}

#[test]
fn finalize_writes_obsidian_record() -> Result<()> {
    with_tmp_home_and_obs(|obs| {
        let uuid = create_test_workspace("obs-ws")?;

        let artifacts = mission_control::mc_data::dismissal::finalize(
            &uuid,
            "# My learning record\n",
            None,
        )?;

        assert!(
            artifacts.obsidian_record.exists(),
            "obsidian record should exist at {:?}",
            artifacts.obsidian_record
        );
        // Should be under obsagents/mc-workspaces/
        let mc_ws_dir = obs.join("mc-workspaces");
        assert!(
            artifacts.obsidian_record.starts_with(&mc_ws_dir),
            "obsidian record should be under mc-workspaces/"
        );
        let content = fs::read_to_string(&artifacts.obsidian_record)?;
        assert!(
            content.contains("My learning record"),
            "obsidian record content should match input"
        );
        Ok(())
    })
}

#[test]
fn finalize_with_proposals_writes_proposals_file() -> Result<()> {
    with_tmp_home_and_obs(|obs| {
        let uuid = create_test_workspace("proposal-ws")?;

        let proposals_content = "- [ ] PATTERN: \"do the thing\"\n  EXPANSION: \"Detailed instructions\"\n";
        let artifacts = mission_control::mc_data::dismissal::finalize(
            &uuid,
            "# Record\n",
            Some(proposals_content),
        )?;

        let proposals_file = artifacts
            .proposals_file
            .expect("proposals_file should be Some when proposal content is provided");
        assert!(
            proposals_file.exists(),
            "proposals file should exist at {:?}",
            proposals_file
        );
        // Should be under Projects/<project>/prompts/proposals/
        let expected_dir = obs
            .join("Projects")
            .join("proposal-ws")
            .join("prompts")
            .join("proposals");
        assert!(
            proposals_file.starts_with(&expected_dir),
            "proposals file should be under Projects/<project>/prompts/proposals/"
        );
        let content = fs::read_to_string(&proposals_file)?;
        assert!(
            content.contains("do the thing"),
            "proposals file should contain the candidate content"
        );
        Ok(())
    })
}

#[test]
fn finalize_leaves_data_intact_when_obsidian_write_fails() -> Result<()> {
    with_tmp_home_and_obs(|_obs| {
        // Create workspace first (HOME is set correctly at this point).
        let uuid = create_test_workspace("safe-ws")?;
        let data_dir = mission_control::mc_data::paths::workspace_dir(&uuid);

        // Now point OBS_AGENTS at a file (not a dir) so create_dir_all fails.
        let bad_obs = data_dir.parent().unwrap().join("not-a-dir.txt");
        fs::write(&bad_obs, "I am a file, not a dir")?;
        let prior_obs = std::env::var_os("OBS_AGENTS");
        unsafe { std::env::set_var("OBS_AGENTS", &bad_obs) };

        let result = mission_control::mc_data::dismissal::finalize(
            &uuid,
            "# Record\n",
            None,
        );

        // Restore OBS_AGENTS
        unsafe {
            match prior_obs {
                Some(v) => std::env::set_var("OBS_AGENTS", v),
                None => std::env::remove_var("OBS_AGENTS"),
            }
        }

        // finalize should have failed (obsidian dir creation failed)
        assert!(result.is_err(), "finalize should fail when obsidian dir cannot be created");
        // Data dir should still be intact — the rename never happened
        assert!(
            data_dir.exists(),
            "data dir should be intact after failed finalize at {:?}",
            data_dir
        );
        Ok(())
    })
}

#[test]
fn finalize_removes_display_symlink() -> Result<()> {
    with_tmp_home_and_obs(|_obs| {
        let uuid = create_test_workspace("link-ws")?;
        let link = mission_control::mc_data::paths::display_symlink("link-ws");
        assert!(link.exists() || link.is_symlink(), "symlink should exist before finalize at {link:?}");

        mission_control::mc_data::dismissal::finalize(
            &uuid,
            "# Record\n",
            None,
        )?;

        // Symlink should be gone after finalize
        assert!(
            !link.exists() && !link.is_symlink(),
            "display symlink should be removed after finalize at {link:?}"
        );
        Ok(())
    })
}
