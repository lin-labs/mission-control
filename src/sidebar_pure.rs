/// Pure, dependency-free helpers for sidebar rendering.
///
/// Kept in the library crate so integration tests can reach them without
/// pulling in the full TUI (which depends on binary-only modules).
use ratatui::{
    style::{Color, Style},
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
    let max_text_cols = sidebar_inner_width.saturating_sub(indent_len).saturating_sub(1);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn workspace_with_description_shows_subtitle_in_dim() {
        let line = description_subtitle_line(
            Some("build the calibration backfill agent"),
            40,
        )
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
