/// Integration tests for the sidebar description-subtitle rendering (Phase 3, Task 2).
///
/// These tests exercise `sidebar_pure::description_subtitle_line`, which is the
/// pure function used by `tui::sidebar::render_sidebar` to produce the optional
/// dim subtitle line below each workspace name.
///
/// Run with: cargo test --test tui_sidebar_render -- --test-threads=1
use mission_control::sidebar_pure::{description_subtitle_line, parse_hex_color};
use ratatui::style::Color;
use ratatui::text::Line;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Collapse a `Line` into its plain-text content (no style info), for easy
/// assertion against expected strings.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[test]
fn workspace_with_description_shows_subtitle_in_dim() {
    // Wide enough that there is no truncation.
    let line = description_subtitle_line(Some("build the calibration backfill agent"), 40)
        .expect("should return Some for non-empty description");

    let text = line_text(&line);

    // Must contain the description text.
    assert!(
        text.contains("build the calibration backfill agent"),
        "expected description in line, got: {:?}",
        text
    );

    // Must be indented by 2 spaces.
    assert!(
        text.starts_with("  "),
        "expected 2-space indent, got: {:?}",
        text
    );

    // The styled span (description text) should use DarkGray.
    use ratatui::style::Color;
    let styled_span = line
        .spans
        .iter()
        .find(|s| s.content.contains("build"))
        .expect("description span not found");
    assert_eq!(
        styled_span.style.fg,
        Some(Color::DarkGray),
        "description span should be DarkGray"
    );
}

#[test]
fn workspace_without_description_has_no_subtitle() {
    // None description → no subtitle.
    let result = description_subtitle_line(None, 40);
    assert!(result.is_none(), "expected None for None description");

    // Empty string → no subtitle.
    let result = description_subtitle_line(Some(""), 40);
    assert!(result.is_none(), "expected None for empty description");

    // Whitespace-only → no subtitle.
    let result = description_subtitle_line(Some("   "), 40);
    assert!(
        result.is_none(),
        "expected None for whitespace-only description"
    );
}

#[test]
fn description_truncates_at_sidebar_width() {
    // sidebar_inner_width = 20
    // indent = 2, ellipsis = 1 → max_text_cols = 17
    // A description of 30 chars should be truncated.
    let long_desc = "abcdefghijklmnopqrstuvwxyz1234";
    let line = description_subtitle_line(Some(long_desc), 20)
        .expect("should return Some for non-empty description");

    let text = line_text(&line);

    // Must end with the ellipsis character.
    assert!(
        text.contains('…'),
        "expected ellipsis in truncated line, got: {:?}",
        text
    );

    // The entire rendered text (including indent) must not exceed sidebar_inner_width.
    // We measure in chars as a proxy for display columns.
    let char_len: usize = text.chars().count();
    assert!(
        char_len <= 20,
        "expected total char count <= 20, got {} in {:?}",
        char_len,
        text
    );

    // The full original text must NOT appear verbatim.
    assert!(
        !text.contains(long_desc),
        "expected truncated text to not contain full description"
    );
}

#[test]
fn description_uses_only_first_line() {
    let multi = "first line\nsecond line";
    let line = description_subtitle_line(Some(multi), 40)
        .expect("should return Some for multi-line description");

    let text = line_text(&line);

    assert!(
        text.contains("first line"),
        "expected first line in output, got: {:?}",
        text
    );
    assert!(
        !text.contains("second line"),
        "expected second line to be absent, got: {:?}",
        text
    );
}

// ── custom_color (cmux workspace tint) ───────────────────────────────────────

#[test]
fn parse_hex_color_handles_real_cmux_values() {
    // Sampled live from `cmux list-workspaces --json --id-format both`:
    assert_eq!(parse_hex_color("#C0392B"), Some(Color::Rgb(192, 57, 43)));
    assert_eq!(parse_hex_color("#006B6B"), Some(Color::Rgb(0, 107, 107)));
    assert_eq!(parse_hex_color("#4A5C18"), Some(Color::Rgb(74, 92, 24)));
}

#[test]
fn parse_hex_color_returns_none_for_invalid() {
    assert_eq!(parse_hex_color(""), None);
    assert_eq!(parse_hex_color("not a color"), None);
    assert_eq!(parse_hex_color("#XYZ"), None);
    assert_eq!(parse_hex_color("#12345"), None); // too short
    assert_eq!(parse_hex_color("#1234567"), None); // too long
}
