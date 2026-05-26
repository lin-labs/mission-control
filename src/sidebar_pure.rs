/// Pure, dependency-free helpers for sidebar rendering.
///
/// Kept in the library crate so integration tests can reach them without
/// pulling in the full TUI (which depends on binary-only modules).
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Build the dim subtitle `Line` for a workspace description, or `None` if there is nothing to
/// show.
///
/// Rules:
/// - Only the first line of `description` is used (split on `\n`).
/// - Leading/trailing whitespace is trimmed.
/// - The result is indented by 2 spaces.
/// - If the text (after indent) would exceed `sidebar_inner_width`, it is truncated with `…`.
/// - Returns `None` when `description` is `None` or blank after trimming.
// Exercised by tests/tui_sidebar_render.rs and module-local tests against the
// lib target; the sidebar render path in the bin doesn't call it yet.
#[allow(dead_code)]
pub fn description_subtitle_line(
    description: Option<&str>,
    sidebar_inner_width: u16,
) -> Option<Line<'static>> {
    let raw = description?;
    let first = raw.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return None;
    }

    // Available columns: inner_width minus 2-space indent, minus 1 for the possible ellipsis.
    // We need at least 1 column for content.
    let indent = "  ";
    let indent_len = 2u16;
    let max_text_cols = sidebar_inner_width
        .saturating_sub(indent_len)
        .saturating_sub(1);
    if max_text_cols == 0 {
        return None;
    }

    // Count Unicode scalar values (chars) as a proxy for display columns (ASCII-safe).
    let char_count = first.chars().count();
    let text: String = if char_count > max_text_cols as usize {
        let truncated: String = first.chars().take(max_text_cols as usize).collect();
        format!("{}…", truncated)
    } else {
        first.to_owned()
    };

    let line = Line::from(vec![
        Span::raw(indent),
        Span::styled(text, Style::default().fg(Color::DarkGray)),
    ]);
    Some(line)
}

/// Parse a `#RRGGBB` hex color string (as emitted by `cmux list-workspaces --json`'s
/// `custom_color` field) into a ratatui `Color::Rgb`. Returns `None` if the string is
/// not a well-formed 6-digit hex with a leading `#`.
///
/// Accepts upper- or lower-case hex digits. Strips surrounding whitespace.
/// Examples: `#C0392B`, `#006B6B`, `#4a5c18`.
pub fn parse_hex_color(s: &str) -> Option<Color> {
    let trimmed = s.trim();
    let hex = trimmed.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

pub fn workspace_accent_color(custom_color: Option<&str>) -> Option<Color> {
    custom_color.and_then(parse_hex_color)
}

pub fn workspace_panel_border_style(
    custom_color: Option<&str>,
    focused: bool,
    focused_fallback: Color,
) -> Style {
    let border_color = workspace_accent_color(custom_color).unwrap_or(if focused {
        focused_fallback
    } else {
        Color::DarkGray
    });

    let mut style = Style::default().fg(border_color);
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn parse_hex_color_accepts_well_formed_uppercase() {
        assert_eq!(
            parse_hex_color("#C0392B"),
            Some(Color::Rgb(0xC0, 0x39, 0x2B))
        );
    }

    #[test]
    fn parse_hex_color_accepts_well_formed_lowercase() {
        assert_eq!(
            parse_hex_color("#4a5c18"),
            Some(Color::Rgb(0x4a, 0x5c, 0x18))
        );
    }

    #[test]
    fn parse_hex_color_trims_whitespace() {
        assert_eq!(
            parse_hex_color("  #006B6B  "),
            Some(Color::Rgb(0x00, 0x6B, 0x6B))
        );
    }

    #[test]
    fn parse_hex_color_rejects_missing_hash() {
        assert_eq!(parse_hex_color("C0392B"), None);
    }

    #[test]
    fn parse_hex_color_rejects_wrong_length() {
        assert_eq!(parse_hex_color("#C0392"), None);
        assert_eq!(parse_hex_color("#C0392BB"), None);
    }

    #[test]
    fn parse_hex_color_rejects_non_hex_chars() {
        assert_eq!(parse_hex_color("#ZZZZZZ"), None);
    }

    #[test]
    fn workspace_panel_border_style_prefers_workspace_accent() {
        let style = workspace_panel_border_style(Some("#C0392B"), false, Color::Cyan);
        assert_eq!(style.fg, Some(Color::Rgb(0xC0, 0x39, 0x2B)));
    }

    #[test]
    fn workspace_panel_border_style_falls_back_to_focus_color() {
        let focused = workspace_panel_border_style(None, true, Color::Cyan);
        let unfocused = workspace_panel_border_style(None, false, Color::Cyan);

        assert_eq!(focused.fg, Some(Color::Cyan));
        assert_eq!(unfocused.fg, Some(Color::DarkGray));
    }

    #[test]
    fn workspace_with_description_shows_subtitle_in_dim() {
        let line = description_subtitle_line(Some("build the calibration backfill agent"), 40)
            .expect("should return Some for non-empty description");

        let text = line_text(&line);

        assert!(
            text.contains("build the calibration backfill agent"),
            "expected description in line, got: {:?}",
            text
        );
        assert!(
            text.starts_with("  "),
            "expected 2-space indent, got: {:?}",
            text
        );

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
        assert!(description_subtitle_line(None, 40).is_none());
        assert!(description_subtitle_line(Some(""), 40).is_none());
        assert!(description_subtitle_line(Some("   "), 40).is_none());
    }

    #[test]
    fn description_truncates_at_sidebar_width() {
        // sidebar_inner_width = 20: indent=2, ellipsis=1 → max_text_cols = 17
        let long_desc = "abcdefghijklmnopqrstuvwxyz1234";
        let line = description_subtitle_line(Some(long_desc), 20)
            .expect("should return Some for non-empty description");

        let text = line_text(&line);

        assert!(
            text.contains('…'),
            "expected ellipsis in truncated line, got: {:?}",
            text
        );

        let char_len: usize = text.chars().count();
        assert!(
            char_len <= 20,
            "expected total char count <= 20, got {} in {:?}",
            char_len,
            text
        );

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
}
