use mission_control::mc_data::prompts::{self, Confidence, PromptRules, Rule};

// ---------------------------------------------------------------------------
// Test-isolation helper: set OBS_AGENTS to a tempdir, restore after the test.
// Run with --test-threads=1 to avoid env-var races.
// ---------------------------------------------------------------------------

fn with_tmp_obsagents<F: FnOnce(&std::path::Path)>(f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("OBS_AGENTS");
    unsafe { std::env::set_var("OBS_AGENTS", tmp.path()) };
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tmp.path())));
    match prior {
        Some(v) => unsafe { std::env::set_var("OBS_AGENTS", v) },
        None => unsafe { std::env::remove_var("OBS_AGENTS") },
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// ---------------------------------------------------------------------------
// Sample rules.md fixture
// ---------------------------------------------------------------------------

const SAMPLE_RULES_MD: &str = r#"# Prompt optimization rules for project predinvest (EXPERIMENTAL — high-churn — may be wrong)

These rules are auto-suggested by mission-control workspace post-mortems and
manually promoted. They are HINTS, not canon.

## Active

- PATTERN: "build … self-improvement"
  EXPANSION: "Use /blin-ralph PRD-driven loop."
  confidence: high  added: 2026-05-23 by predinvest  last-fired: 2026-05-23  hits: 4

- PATTERN: "make X composable"
  EXPANSION: "Define typing.Protocol interfaces"
  confidence: med   added: 2026-05-23 by predinvest  last-fired: 2026-05-23  hits: 2

## Stale (unused ≥ 30 days — review and delete)

- PATTERN: "old pattern"
  EXPANSION: "old expansion"
  confidence: low  added: 2025-01-01 by predinvest  last-fired: 2025-01-15  hits: 1

"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn parse_rules_md_extracts_active_and_stale() {
    let rules = PromptRules::parse(SAMPLE_RULES_MD, "predinvest").unwrap();
    assert_eq!(rules.project, "predinvest");
    assert_eq!(rules.active.len(), 2, "should have 2 active rules");
    assert_eq!(rules.stale.len(), 1, "should have 1 stale rule");

    let r0 = &rules.active[0];
    assert_eq!(r0.pattern, "build … self-improvement");
    assert_eq!(r0.expansion, "Use /blin-ralph PRD-driven loop.");
    assert!(matches!(r0.confidence, Confidence::High));
    assert_eq!(r0.added, "2026-05-23");
    assert_eq!(r0.added_by, "predinvest");
    assert_eq!(r0.last_fired.as_deref(), Some("2026-05-23"));
    assert_eq!(r0.hits, 4);

    let r1 = &rules.active[1];
    assert_eq!(r1.pattern, "make X composable");
    assert!(matches!(r1.confidence, Confidence::Med));
    assert_eq!(r1.hits, 2);

    let rs = &rules.stale[0];
    assert_eq!(rs.pattern, "old pattern");
    assert!(matches!(rs.confidence, Confidence::Low));
}

#[test]
fn roundtrip_parse_to_markdown_to_parse() {
    let rules = PromptRules::parse(SAMPLE_RULES_MD, "predinvest").unwrap();
    let md = rules.to_markdown();
    // Re-parse the rendered markdown
    let rules2 = PromptRules::parse(&md, "predinvest").unwrap();

    assert_eq!(rules2.active.len(), rules.active.len());
    assert_eq!(rules2.stale.len(), rules.stale.len());
    for (a, b) in rules.active.iter().zip(rules2.active.iter()) {
        assert_eq!(a.pattern, b.pattern);
        assert_eq!(a.expansion, b.expansion);
        assert_eq!(a.hits, b.hits);
        assert_eq!(a.last_fired, b.last_fired);
        assert_eq!(a.added, b.added);
        assert_eq!(a.added_by, b.added_by);
    }
    for (a, b) in rules.stale.iter().zip(rules2.stale.iter()) {
        assert_eq!(a.pattern, b.pattern);
        assert_eq!(a.expansion, b.expansion);
    }
}

#[test]
fn obsagents_root_honors_env_var() {
    with_tmp_obsagents(|tmp| {
        let got = prompts::obsagents_root();
        assert_eq!(got, tmp, "obsagents_root() should return the OBS_AGENTS env var value");
    });
}

#[test]
fn parse_proposal_file_returns_only_ticked() {
    let proposal = r#"# Prompt-optimization candidates — predinvest 2026-05-23

Rules:

- [x] PATTERN: "ticked rule one"
      EXPANSION: "Do X when Y"
      confidence: high
      evidence: events 1, 2

- [ ] PATTERN: "unticked rule"
      EXPANSION: "Do Z"
      confidence: med

- [x] PATTERN: "ticked rule two"
      EXPANSION: "Do A when B"
      confidence: low
"#;

    let ticked = prompts::parse_proposal_file(proposal).unwrap();
    assert_eq!(ticked.len(), 2, "only [x] rules should be returned, got: {:?}", ticked.iter().map(|r| &r.pattern).collect::<Vec<_>>());
    assert_eq!(ticked[0].pattern, "ticked rule one");
    assert_eq!(ticked[0].expansion, "Do X when Y");
    assert!(matches!(ticked[0].confidence, Confidence::High));
    assert_eq!(ticked[1].pattern, "ticked rule two");
    assert_eq!(ticked[1].expansion, "Do A when B");
    assert!(matches!(ticked[1].confidence, Confidence::Low));
}

#[test]
fn load_on_missing_project_returns_empty() {
    with_tmp_obsagents(|_| {
        let rules = PromptRules::load("nonexistent-project").unwrap();
        assert_eq!(rules.project, "nonexistent-project");
        assert!(rules.active.is_empty());
        assert!(rules.stale.is_empty());
    });
}

#[test]
fn save_and_load_roundtrip() {
    with_tmp_obsagents(|_| {
        let rules = PromptRules {
            project: "testproj".to_string(),
            active: vec![
                Rule {
                    pattern: "test pattern".to_string(),
                    expansion: "test expansion".to_string(),
                    confidence: Confidence::High,
                    added: "2026-05-23".to_string(),
                    added_by: "myworkspace".to_string(),
                    last_fired: Some("2026-05-23".to_string()),
                    hits: 7,
                },
            ],
            stale: vec![],
        };
        rules.save().unwrap();

        let loaded = PromptRules::load("testproj").unwrap();
        assert_eq!(loaded.active.len(), 1);
        let r = &loaded.active[0];
        assert_eq!(r.pattern, "test pattern");
        assert_eq!(r.expansion, "test expansion");
        assert!(matches!(r.confidence, Confidence::High));
        assert_eq!(r.added, "2026-05-23");
        assert_eq!(r.added_by, "myworkspace");
        assert_eq!(r.last_fired.as_deref(), Some("2026-05-23"));
        assert_eq!(r.hits, 7);
    });
}

#[test]
fn rule_id_is_stable_and_deterministic() {
    // Same input → same output on repeated calls
    let id1 = prompts::rule_id("build … self-improvement");
    let id2 = prompts::rule_id("build … self-improvement");
    assert_eq!(id1, id2, "rule_id must be deterministic");
    // Different input → different id (with extremely high probability)
    let id3 = prompts::rule_id("make X composable");
    assert_ne!(id1, id3, "different patterns should produce different ids");
    // Has the expected length (12 hex chars)
    assert_eq!(id1.len(), 12, "rule_id should be 12 hex chars");
}
