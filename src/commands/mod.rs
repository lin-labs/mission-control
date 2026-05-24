//! Command registry for mc's vim-like `:command` bar.
//!
//! v1 ships exactly one command (`summarize`). The registry is a static slice
//! so adding more is a one-line change. Prefix-only matching, no fuzzy logic.

pub mod summarize;

/// A command the user can invoke from the `:command` bar.
pub struct CommandSpec {
    pub name: &'static str,
    pub help: &'static str,
}

pub const COMMANDS: &[CommandSpec] = &[CommandSpec {
    name: "summarize",
    help: "snapshot all workspaces to obsidian",
}];

/// All command names with `prefix` as a prefix (strict prefix, includes exact match).
/// Returns names sorted alphabetically for stable UI ordering.
pub fn matches(prefix: &str) -> Vec<&'static str> {
    let mut hits: Vec<&'static str> = COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(prefix))
        .map(|c| c.name)
        .collect();
    hits.sort_unstable();
    hits
}

/// Longest common prefix among the given strings, or empty if the list is empty.
pub fn longest_common_prefix(names: &[&'static str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut end = first.len();
    for s in &names[1..] {
        let mut i = 0;
        for (a, b) in first.bytes().zip(s.bytes()) {
            if a != b {
                break;
            }
            i += 1;
        }
        end = end.min(i);
    }
    first[..end].to_string()
}

/// Result posted back to the UI from a background command handler.
#[derive(Debug, Clone)]
pub enum CommandResult {
    /// `summarize` finished successfully. Holds the absolute path written.
    SummarizeDone(std::path::PathBuf),
    /// The command failed. Holds a short human-readable reason.
    Err(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact() {
        assert_eq!(matches("summarize"), vec!["summarize"]);
    }

    #[test]
    fn matches_prefix() {
        assert_eq!(matches("sum"), vec!["summarize"]);
    }

    #[test]
    fn matches_empty_prefix_returns_all() {
        assert_eq!(matches(""), vec!["summarize"]);
    }

    #[test]
    fn matches_no_hit() {
        assert!(matches("zzz").is_empty());
    }

    #[test]
    fn lcp_single() {
        assert_eq!(longest_common_prefix(&["summarize"]), "summarize");
    }

    #[test]
    fn lcp_two() {
        assert_eq!(longest_common_prefix(&["summary", "summarize"]), "summar");
    }

    #[test]
    fn lcp_empty() {
        assert_eq!(longest_common_prefix(&[]), "");
    }
}
