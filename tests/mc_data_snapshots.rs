use mission_control::mc_data::{paths, snapshots, trajectory::TrajectoryDoc};

// Run with --test-threads=1 to avoid concurrent HOME mutations.

fn with_tmp_home<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", tmp.path()) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp.path())));
    match prior {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn minimal_doc() -> TrajectoryDoc {
    let mut doc = TrajectoryDoc::default();
    doc.ensure_sections();
    doc
}

#[test]
fn write_snapshot_creates_file_with_markdown() {
    with_tmp_home(|_| {
        let uuid = "snap-uuid-1";
        let doc = minimal_doc();
        let path = snapshots::write_snapshot(uuid, 5, &doc).expect("write_snapshot");

        assert!(path.exists(), "snapshot file should exist at {path:?}");
        assert!(path.ends_with("trajectory-5.md"));

        let contents = std::fs::read_to_string(&path).expect("read snapshot");
        // The markdown output should contain the section headers.
        assert!(contents.contains("## Goal"), "contents: {contents}");
        assert!(
            contents.contains("## Tasks & Progress"),
            "contents: {contents}"
        );
    });
}

#[test]
fn write_snapshot_path_is_inside_histories_dir() {
    with_tmp_home(|_| {
        let uuid = "snap-uuid-2";
        let doc = minimal_doc();
        let path = snapshots::write_snapshot(uuid, 3, &doc).expect("write_snapshot");

        let expected_dir = paths::histories_dir(uuid);
        assert_eq!(path.parent().unwrap(), expected_dir);
    });
}

#[test]
fn highest_snapshot_returns_zero_when_empty() {
    with_tmp_home(|_| {
        let uuid = "snap-uuid-empty";
        let n = snapshots::highest_snapshot(uuid).expect("highest_snapshot");
        assert_eq!(n, 0);
    });
}

#[test]
fn highest_snapshot_tracks_highest_n() {
    with_tmp_home(|_| {
        let uuid = "snap-uuid-multi";
        let doc = minimal_doc();
        snapshots::write_snapshot(uuid, 1, &doc).expect("write n=1");
        snapshots::write_snapshot(uuid, 3, &doc).expect("write n=3");

        let n = snapshots::highest_snapshot(uuid).expect("highest_snapshot");
        assert_eq!(n, 3);
    });
}

#[test]
fn non_conforming_files_in_histories_are_ignored() {
    with_tmp_home(|_| {
        let uuid = "snap-uuid-noise";
        let doc = minimal_doc();
        snapshots::write_snapshot(uuid, 2, &doc).expect("write n=2");

        // Write a file that does not match the trajectory-N.md pattern.
        let hist_dir = paths::histories_dir(uuid);
        std::fs::write(hist_dir.join("README.txt"), "ignored").expect("write noise");
        std::fs::write(hist_dir.join("trajectory-.md"), "ignored").expect("write malformed");
        std::fs::write(hist_dir.join("trajectory-abc.md"), "ignored").expect("write non-numeric");

        let n = snapshots::highest_snapshot(uuid).expect("highest_snapshot");
        assert_eq!(n, 2, "noise files should not affect highest_snapshot");
    });
}

#[test]
fn snapshot_content_matches_doc_to_markdown() {
    with_tmp_home(|_| {
        let uuid = "snap-uuid-content";
        let doc = minimal_doc();
        let expected = doc.to_markdown();

        let path = snapshots::write_snapshot(uuid, 1, &doc).expect("write_snapshot");
        let got = std::fs::read_to_string(&path).expect("read snapshot");

        assert_eq!(got, expected);
    });
}
