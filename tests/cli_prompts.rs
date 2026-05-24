use mission_control::mc_data::prompts::{self, PromptRules};
use std::fs;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn with_tmp_obsagents<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("OBS_AGENTS");
    unsafe { std::env::set_var("OBS_AGENTS", tmp.path()) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp.path())));
    match prior {
        Some(v) => unsafe { std::env::set_var("OBS_AGENTS", v) },
        None => unsafe { std::env::remove_var("OBS_AGENTS") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// ---------------------------------------------------------------------------
// promote_rules smoke test
// ---------------------------------------------------------------------------

#[test]
fn promote_rules_creates_rules_md() {
    with_tmp_obsagents(|obsagents_root| {
        // Create the proposals directory tree inside the temp obsagents root
        let proposals_dir = obsagents_root
            .join("Projects")
            .join("myproject")
            .join("prompts")
            .join("proposals");
        fs::create_dir_all(&proposals_dir).unwrap();

        let proposal_path = proposals_dir.join("2026-05-23-myproject.md");
        let proposal_content = r#"# Prompt-optimization candidates — myproject 2026-05-23

Rules:

- [x] PATTERN: "build self-improvement"
      EXPANSION: "Use blin-ralph loop"
      confidence: high
      evidence: events 1, 2

- [ ] PATTERN: "unticked one"
      EXPANSION: "Do nothing"
      confidence: low
"#;
        fs::write(&proposal_path, proposal_content).unwrap();

        // Run promote_rules
        mission_control::cli::promote_rules::run(&proposal_path).unwrap();

        // rules.md should now exist under the project
        let rules_path = prompts::rules_path("myproject");
        assert!(
            rules_path.exists(),
            "rules.md should have been created at {rules_path:?}"
        );

        // Load and verify
        let rules = PromptRules::load("myproject").unwrap();
        assert_eq!(rules.active.len(), 1, "only 1 ticked rule promoted");
        assert_eq!(rules.active[0].pattern, "build self-improvement");
        assert_eq!(rules.active[0].expansion, "Use blin-ralph loop");

        // Proposal file should be archived
        assert!(
            !proposal_path.exists(),
            "proposal file should have been moved to .archived/"
        );
        let archived = proposals_dir
            .join(".archived")
            .join("2026-05-23-myproject.md");
        assert!(
            archived.exists(),
            "archived file should exist at {archived:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// record_hit smoke test
// ---------------------------------------------------------------------------

#[test]
fn record_hit_increments_hits_and_updates_last_fired() {
    with_tmp_obsagents(|_| {
        use mission_control::mc_data::prompts::{Confidence, Rule};

        // Seed a rules.md
        let initial = PromptRules {
            project: "hitproject".to_string(),
            active: vec![Rule {
                pattern: "some pattern to hit".to_string(),
                expansion: "some expansion".to_string(),
                confidence: Confidence::Med,
                added: "2026-01-01".to_string(),
                added_by: "ws".to_string(),
                last_fired: Some("2026-01-01".to_string()),
                hits: 3,
            }],
            stale: vec![],
        };
        initial.save().unwrap();

        // Call record_hit
        let id = prompts::rule_id("some pattern to hit");
        mission_control::cli::record_hit::run("hitproject", &id).unwrap();

        // Reload and verify
        let rules = PromptRules::load("hitproject").unwrap();
        assert_eq!(rules.active.len(), 1);
        let r = &rules.active[0];
        assert_eq!(r.hits, 4, "hits should be bumped to 4");
        // last-fired should be today's date
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(r.last_fired.as_deref(), Some(today.as_str()));
    });
}

// ---------------------------------------------------------------------------
// gc smoke test
// ---------------------------------------------------------------------------

#[test]
fn gc_moves_stale_rules_and_marks_old_ones() {
    with_tmp_obsagents(|_| {
        use mission_control::mc_data::prompts::{Confidence, Rule};

        // Seed a project with one rule that fired 40 days ago (> 30d → stale)
        // and one that fired 65 days ago (> 60d → stale AND marked for deletion)
        let forty_days_ago = (chrono::Local::now() - chrono::Duration::days(40))
            .format("%Y-%m-%d")
            .to_string();
        let sixty_five_days_ago = (chrono::Local::now() - chrono::Duration::days(65))
            .format("%Y-%m-%d")
            .to_string();

        let initial = PromptRules {
            project: "gcproject".to_string(),
            active: vec![
                Rule {
                    pattern: "stale pattern".to_string(),
                    expansion: "stale expansion".to_string(),
                    confidence: Confidence::Med,
                    added: "2026-01-01".to_string(),
                    added_by: "ws".to_string(),
                    last_fired: Some(forty_days_ago.clone()),
                    hits: 2,
                },
                Rule {
                    pattern: "very stale pattern".to_string(),
                    expansion: "very stale expansion".to_string(),
                    confidence: Confidence::Low,
                    added: "2026-01-01".to_string(),
                    added_by: "ws".to_string(),
                    last_fired: Some(sixty_five_days_ago.clone()),
                    hits: 1,
                },
                Rule {
                    pattern: "fresh pattern".to_string(),
                    expansion: "fresh expansion".to_string(),
                    confidence: Confidence::High,
                    added: "2026-05-23".to_string(),
                    added_by: "ws".to_string(),
                    last_fired: Some("2026-05-23".to_string()),
                    hits: 5,
                },
            ],
            stale: vec![],
        };
        initial.save().unwrap();

        mission_control::cli::gc::run().unwrap();

        let rules = PromptRules::load("gcproject").unwrap();

        // Only the fresh rule should remain active
        assert_eq!(
            rules.active.len(),
            1,
            "only fresh rule should remain active"
        );
        assert_eq!(rules.active[0].pattern, "fresh pattern");

        // Both old rules moved to stale
        assert_eq!(rules.stale.len(), 2, "both old rules should be in stale");

        // The very-stale rule (>60 days) should be marked for deletion
        let very_stale = rules
            .stale
            .iter()
            .find(|r| r.pattern == "very stale pattern")
            .expect("very stale rule should be in stale section");
        assert!(
            very_stale.expansion.contains("# TODO: review for deletion"),
            "very stale rule should be marked for deletion, expansion: {:?}",
            very_stale.expansion
        );

        // The 40-day-stale rule should NOT be marked for deletion
        let stale_rule = rules
            .stale
            .iter()
            .find(|r| r.pattern == "stale pattern")
            .expect("stale pattern should be in stale section");
        assert!(
            !stale_rule.expansion.contains("# TODO: review for deletion"),
            "40-day stale rule should not be marked for deletion"
        );
    });
}
