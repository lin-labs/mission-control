use mission_control::mc_data::session_log::{
    WorkspaceContext, latest_session_file_for_workspace_in_dir,
    resolve_session_log_for_surface_in_dir,
};
use std::fs;

fn with_tmp_histories<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().unwrap();
    let histories = tmp.path().join("histories");
    fs::create_dir_all(&histories).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&histories)));
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn write_session(histories: &std::path::Path, name: &str, host: &str, cwd: &str, uuid: &str) {
    let frontmatter = format!(
        "---\ndate: 2026-05-24\nhost: {host}\ncwd: {cwd}\nworkspace_id: {uuid}\n---\n\n## 12:00 PT \u{2014} boyan\nhello\n"
    );
    fs::write(histories.join(name), frontmatter).unwrap();
}

#[test]
fn picks_log_with_matching_cwd_prefix_over_more_recent_without() {
    with_tmp_histories(|histories| {
        // A: older log with matching cwd (wrong workspace_id)
        write_session(
            histories,
            "a.md",
            "mbp",
            "/Users/blin/Projects/agents/skills",
            "WRONG-UUID",
        );
        // B: newer log with workspace_id match but wrong cwd
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_session(
            histories,
            "b.md",
            "mbp",
            "/Users/blin/Tools/mission-control",
            "TARGET-UUID",
        );

        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked =
            latest_session_file_for_workspace_in_dir(histories, "TARGET-UUID", &ctx).unwrap();
        let p = picked.unwrap();
        assert!(
            p.ends_with("a.md"),
            "should pick by cwd ancestry, got {p:?}"
        );
    });
}

#[test]
fn excludes_logs_with_different_host_even_if_uuid_matches() {
    with_tmp_histories(|histories| {
        // Only uuid-match candidate is on wrong host.
        write_session(histories, "a.md", "labs", "/home/blin/x", "TARGET-UUID");
        // Tier 1 candidate on matching host with matching cwd.
        write_session(
            histories,
            "b.md",
            "mbp",
            "/Users/blin/Projects/agents",
            "OTHER-UUID",
        );
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked =
            latest_session_file_for_workspace_in_dir(histories, "TARGET-UUID", &ctx).unwrap();
        let p = picked.unwrap();
        assert!(
            p.ends_with("b.md"),
            "tier 1 (host+cwd) should beat tier 2 (uuid only): {p:?}"
        );
    });
}

#[test]
fn falls_back_to_uuid_when_no_host_cwd_match() {
    with_tmp_histories(|histories| {
        // Only log has matching uuid, mismatched host and no cwd ancestry.
        write_session(histories, "a.md", "labs", "/elsewhere", "TARGET-UUID");
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked =
            latest_session_file_for_workspace_in_dir(histories, "TARGET-UUID", &ctx).unwrap();
        let p = picked.unwrap();
        assert!(p.ends_with("a.md"), "fallback to uuid match: {p:?}");
    });
}

#[test]
fn picks_most_specific_cwd_when_multiple_match() {
    with_tmp_histories(|histories| {
        // shallow.md: cwd=/Users/blin -- NOT a descendant of ctx.cwd=/Users/blin/Projects/agents
        write_session(histories, "shallow.md", "mbp", "/Users/blin", "X");
        std::thread::sleep(std::time::Duration::from_millis(20));
        // deep.md: cwd=/Users/blin/Projects/agents/skills -- IS a descendant
        write_session(
            histories,
            "deep.md",
            "mbp",
            "/Users/blin/Projects/agents/skills",
            "X",
        );
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/Projects/agents".into()),
        };
        let picked = latest_session_file_for_workspace_in_dir(histories, "X", &ctx).unwrap();
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
    with_tmp_histories(|histories| {
        write_session(histories, "older.md", "mbp", "/Users/blin/x", "X");
        std::thread::sleep(std::time::Duration::from_millis(50));
        write_session(histories, "newer.md", "mbp", "/Users/blin/x", "X");
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/Users/blin/x".into()),
        };
        let picked = latest_session_file_for_workspace_in_dir(histories, "X", &ctx)
            .unwrap()
            .unwrap();
        assert!(picked.ends_with("newer.md"));
    });
}

#[test]
fn no_match_returns_none() {
    with_tmp_histories(|histories| {
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/nope".into()),
        };
        let picked = latest_session_file_for_workspace_in_dir(histories, "no-uuid", &ctx).unwrap();
        assert!(picked.is_none());
    });
}

#[test]
fn surface_resolution_distributes_workspace_id_matches_across_repos_before_cwd_tier() {
    with_tmp_histories(|histories| {
        write_session(histories, "repo-a.md", "mbp", "/tmp/repo-a", "TARGET-UUID");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_session(histories, "repo-b.md", "mbp", "/tmp/repo-b", "TARGET-UUID");
        let ctx = WorkspaceContext {
            host: Some("mbp".into()),
            cwd: Some("/tmp/repo-a".into()),
        };

        let first = resolve_session_log_for_surface_in_dir(
            histories,
            "TARGET-UUID",
            "surface:1",
            &ctx,
            Some("claude"),
            0,
        )
        .unwrap()
        .unwrap();
        let second = resolve_session_log_for_surface_in_dir(
            histories,
            "TARGET-UUID",
            "surface:2",
            &ctx,
            Some("claude"),
            1,
        )
        .unwrap()
        .unwrap();

        assert_eq!(first.frontmatter.cwd.as_deref(), Some("/tmp/repo-b"));
        assert_eq!(second.frontmatter.cwd.as_deref(), Some("/tmp/repo-a"));
    });
}
