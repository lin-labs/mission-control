use mission_control::mc_data::session_log::{latest_session_file_for_workspace, WorkspaceContext};
use std::fs;

fn with_tmp_obs<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().unwrap();
    let obs = tmp.path().join("obs");
    fs::create_dir_all(obs.join("Sessions")).unwrap();
    let prior = std::env::var_os("OBS_AGENTS");
    unsafe {
        std::env::set_var("OBS_AGENTS", &obs);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&obs)));
    match prior {
        Some(v) => unsafe { std::env::set_var("OBS_AGENTS", v) },
        None => unsafe { std::env::remove_var("OBS_AGENTS") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn write_session(obs: &std::path::Path, name: &str, host: &str, cwd: &str, uuid: &str) {
    let frontmatter = format!(
        "---\ndate: 2026-05-24\nhost: {host}\ncwd: {cwd}\nworkspace_id: {uuid}\n---\n\n## 12:00 PT \u{2014} boyan\nhello\n"
    );
    fs::write(obs.join("Sessions").join(name), frontmatter).unwrap();
}

#[test]
fn picks_log_with_matching_cwd_prefix_over_more_recent_without() {
    with_tmp_obs(|obs| {
        // A: older log with matching cwd (wrong workspace_id)
        write_session(
            obs,
            "a.md",
            "mbp",
            "/Users/blin/Projects/agents/skills",
            "WRONG-UUID",
        );
        // B: newer log with workspace_id match but wrong cwd
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_session(
            obs,
            "b.md",
            "mbp",
            "/Users/blin/Tools/mission-control",
            "TARGET-UUID",
        );

        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked = latest_session_file_for_workspace("TARGET-UUID", &ctx).unwrap();
        let p = picked.unwrap();
        assert!(p.ends_with("a.md"), "should pick by cwd ancestry, got {p:?}");
    });
}

#[test]
fn excludes_logs_with_different_host_even_if_uuid_matches() {
    with_tmp_obs(|obs| {
        // Only uuid-match candidate is on wrong host.
        write_session(obs, "a.md", "labs", "/home/blin/x", "TARGET-UUID");
        // Tier 1 candidate on matching host with matching cwd.
        write_session(obs, "b.md", "mbp", "/Users/blin/Projects/agents", "OTHER-UUID");
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked = latest_session_file_for_workspace("TARGET-UUID", &ctx).unwrap();
        let p = picked.unwrap();
        assert!(
            p.ends_with("b.md"),
            "tier 1 (host+cwd) should beat tier 2 (uuid only): {p:?}"
        );
    });
}

#[test]
fn falls_back_to_uuid_when_no_host_cwd_match() {
    with_tmp_obs(|obs| {
        // Only log has matching uuid, mismatched host and no cwd ancestry.
        write_session(obs, "a.md", "labs", "/elsewhere", "TARGET-UUID");
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked = latest_session_file_for_workspace("TARGET-UUID", &ctx).unwrap();
        let p = picked.unwrap();
        assert!(p.ends_with("a.md"), "fallback to uuid match: {p:?}");
    });
}

#[test]
fn picks_most_specific_cwd_when_multiple_match() {
    with_tmp_obs(|obs| {
        // shallow.md: cwd=/Users/blin -- NOT a descendant of ctx.cwd=/Users/blin/Projects/agents
        write_session(obs, "shallow.md", "mbp", "/Users/blin", "X");
        std::thread::sleep(std::time::Duration::from_millis(20));
        // deep.md: cwd=/Users/blin/Projects/agents/skills -- IS a descendant
        write_session(obs, "deep.md", "mbp", "/Users/blin/Projects/agents/skills", "X");
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked = latest_session_file_for_workspace("X", &ctx).unwrap();
        // Note: ctx.cwd = /Users/blin/Projects/agents
        // - shallow.md cwd=/Users/blin → not a descendant of ctx.cwd, EXCLUDED
        // - deep.md cwd=/Users/blin/Projects/agents/skills → descendant of ctx.cwd, INCLUDED
        // → deep.md wins.
        let p = picked.unwrap();
        assert!(p.ends_with("deep.md"), "most-specific cwd wins: {p:?}");
    });
}

#[test]
fn ties_within_same_cwd_match_break_by_mtime() {
    with_tmp_obs(|obs| {
        write_session(obs, "older.md", "mbp", "/Users/blin/x", "X");
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_session(obs, "newer.md", "mbp", "/Users/blin/x", "X");
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/x".into()),
        };
        let picked = latest_session_file_for_workspace("X", &ctx)
            .unwrap()
            .unwrap();
        assert!(picked.ends_with("newer.md"));
    });
}

#[test]
fn no_match_returns_none() {
    with_tmp_obs(|_| {
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/nope".into()),
        };
        let picked = latest_session_file_for_workspace("no-uuid", &ctx).unwrap();
        assert!(picked.is_none());
    });
}
