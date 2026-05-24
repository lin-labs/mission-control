use mission_control::mc_data::{
    inputs::{self, InputContext},
    paths,
};

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

#[test]
fn write_input_creates_file_with_formatted_sections() {
    with_tmp_home(|_| {
        let uuid = "inp-uuid-1";
        let ctx = InputContext {
            user_why: Some("codex finished sprint-01".to_string()),
            current_screen_tail: Some("screen line 1\nscreen line 2".to_string()),
            last_user_prompt: Some("Let's run calibration backfill".to_string()),
            last_agent_output_tail: Some("agent output here".to_string()),
            edited_sections: vec!["Tasks & Progress".to_string()],
        };

        let path = inputs::write_input(uuid, 7, &ctx).expect("write_input");

        assert!(path.exists(), "input file should exist at {path:?}");
        assert!(
            path.ends_with("7.txt"),
            "filename should be 7.txt, got {path:?}"
        );

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(
            contents.contains("## User context"),
            "missing user context header"
        );
        assert!(
            contents.contains("why: codex finished sprint-01"),
            "missing why line"
        );
        assert!(
            contents.contains("## Auto context (captured at edit-start)"),
            "missing auto context header"
        );
        assert!(
            contents.contains("current-screen-tail:"),
            "missing screen tail"
        );
        assert!(
            contents.contains("  screen line 1"),
            "screen line should be indented"
        );
        assert!(
            contents.contains("last-user-prompt:"),
            "missing last user prompt"
        );
        assert!(
            contents.contains("last-agent-output-tail:"),
            "missing agent output tail"
        );
        assert!(
            contents.contains("edited-sections: [Tasks & Progress]"),
            "missing edited sections"
        );
    });
}

#[test]
fn write_input_path_is_inside_inputs_dir() {
    with_tmp_home(|_| {
        let uuid = "inp-uuid-2";
        let ctx = InputContext::default();
        let path = inputs::write_input(uuid, 3, &ctx).expect("write_input");

        let expected_dir = paths::inputs_dir(uuid);
        assert_eq!(path.parent().unwrap(), expected_dir);
    });
}

#[test]
fn empty_user_why_produces_header_without_why_line() {
    with_tmp_home(|_| {
        let uuid = "inp-uuid-empty-why";
        let ctx = InputContext {
            user_why: None,
            ..Default::default()
        };

        let path = inputs::write_input(uuid, 1, &ctx).expect("write_input");
        let contents = std::fs::read_to_string(&path).expect("read");

        assert!(
            contents.contains("## User context"),
            "user context header must exist"
        );
        assert!(
            !contents.contains("why:"),
            "no why: line when user_why is None, got: {contents}"
        );
    });
}

#[test]
fn auto_context_block_always_exists() {
    with_tmp_home(|_| {
        let uuid = "inp-uuid-auto";
        // Even with a completely default (empty) InputContext, the auto context
        // section header should still be present.
        let ctx = InputContext::default();
        let path = inputs::write_input(uuid, 1, &ctx).expect("write_input");
        let contents = std::fs::read_to_string(&path).expect("read");

        assert!(
            contents.contains("## Auto context (captured at edit-start)"),
            "auto context section must always be present, got: {contents}"
        );
    });
}

#[test]
fn edited_sections_formats_as_list() {
    with_tmp_home(|_| {
        let uuid = "inp-uuid-sections";
        let ctx = InputContext {
            edited_sections: vec!["Goal".to_string(), "Tasks & Progress".to_string()],
            ..Default::default()
        };
        let path = inputs::write_input(uuid, 1, &ctx).expect("write_input");
        let contents = std::fs::read_to_string(&path).expect("read");

        assert!(
            contents.contains("edited-sections: [Goal, Tasks & Progress]"),
            "edited-sections list format, got: {contents}"
        );
    });
}

#[test]
fn to_text_empty_context_has_both_sections() {
    let ctx = InputContext::default();
    let text = ctx.to_text();
    assert!(text.contains("## User context"), "user context header");
    assert!(
        text.contains("## Auto context (captured at edit-start)"),
        "auto context header"
    );
    // No why line, no optional fields.
    assert!(!text.contains("why:"), "no why: for empty context");
    assert!(
        !text.contains("current-screen-tail:"),
        "no screen tail for empty context"
    );
    assert!(
        !text.contains("last-user-prompt:"),
        "no user prompt for empty context"
    );
    assert!(
        !text.contains("last-agent-output-tail:"),
        "no agent output for empty context"
    );
    assert!(
        !text.contains("edited-sections:"),
        "no edited sections for empty context"
    );
}
