use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Med,
    High,
}

impl Confidence {
    fn as_str(&self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Med => "med",
            Confidence::High => "high",
        }
    }

    fn parse(s: &str) -> Confidence {
        match s.trim().to_lowercase().as_str() {
            "high" => Confidence::High,
            "low" => Confidence::Low,
            _ => Confidence::Med,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub pattern: String,
    pub expansion: String,
    pub confidence: Confidence,
    /// YYYY-MM-DD
    pub added: String,
    /// workspace name
    pub added_by: String,
    /// YYYY-MM-DD
    pub last_fired: Option<String>,
    pub hits: u32,
}

#[derive(Debug, Clone)]
pub struct PromptRules {
    pub project: String,
    pub active: Vec<Rule>,
    pub stale: Vec<Rule>,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Resolve the obsAgents vault root (the `Agents/` folder inside the Obsidian
/// vault). Prefers the $OBS_AGENTS env var, falls back to the stable
/// ~/agents/obsAgents symlink (-> obs/Agents) — never a hardcoded iCloud path,
/// and never the nonexistent ~/agents/Obsidian path.
pub fn obsagents_root() -> PathBuf {
    if let Ok(v) = std::env::var("OBS_AGENTS") {
        return PathBuf::from(v);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join("agents/obsAgents")
}

pub fn project_prompts_dir(project: &str) -> PathBuf {
    obsagents_root()
        .join("Projects")
        .join(project)
        .join("prompts")
}

pub fn rules_path(project: &str) -> PathBuf {
    project_prompts_dir(project).join("rules.md")
}

// ---------------------------------------------------------------------------
// rule_id: stable short hash of a pattern string
// ---------------------------------------------------------------------------

/// Return a stable 12-char hex hash of `pattern`.
/// Uses `DefaultHasher` (SipHash) which is stable within a process but NOT
/// across Rust versions/compilations — however for our CLI use-case (the
/// hash is stored and compared within the same binary build) this is fine.
/// We seed it to improve stability.
pub fn rule_id(pattern: &str) -> String {
    let mut h = DefaultHasher::new();
    pattern.hash(&mut h);
    // Use two rounds to get 16 hex chars, take first 12.
    let v = h.finish();
    format!("{:016x}", v)[..12].to_string()
}

// ---------------------------------------------------------------------------
// Markdown serialisation
// ---------------------------------------------------------------------------

impl Rule {
    /// Render to the compact 3-line block format used inside rules.md.
    pub fn to_markdown_block(&self) -> String {
        let last_fired = self.last_fired.as_deref().unwrap_or("never");
        format!(
            "- PATTERN: \"{}\"\n  EXPANSION: \"{}\"\n  confidence: {}  added: {} by {}  last-fired: {}  hits: {}",
            self.pattern,
            self.expansion,
            self.confidence.as_str(),
            self.added,
            self.added_by,
            last_fired,
            self.hits,
        )
    }
}

impl PromptRules {
    /// Render to a full rules.md markdown document.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# Prompt optimization rules for project {} (EXPERIMENTAL — high-churn — may be wrong)\n\n",
            self.project
        ));
        out.push_str(
            "These rules are auto-suggested by mission-control workspace post-mortems and\n\
             manually promoted. They are HINTS, not canon.\n",
        );
        out.push_str("\n## Active\n\n");
        if self.active.is_empty() {
            out.push_str("(none)\n");
        } else {
            for rule in &self.active {
                out.push_str(&rule.to_markdown_block());
                out.push_str("\n\n");
            }
        }
        out.push_str("## Stale (unused ≥ 30 days — review and delete)\n\n");
        if self.stale.is_empty() {
            out.push_str("(none)\n");
        } else {
            for rule in &self.stale {
                out.push_str(&rule.to_markdown_block());
                out.push_str("\n\n");
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Parse
    // -----------------------------------------------------------------------

    /// Parse a rules.md document. Returns an empty PromptRules on minimal/blank content.
    pub fn parse(text: &str, project: &str) -> Result<Self> {
        let active = parse_rules_section(text, "## Active");
        let stale = parse_rules_section(text, "## Stale");
        Ok(PromptRules {
            project: project.to_string(),
            active,
            stale,
        })
    }

    // -----------------------------------------------------------------------
    // Load / Save
    // -----------------------------------------------------------------------

    /// Load rules.md for `project`. Returns an empty PromptRules if the file
    /// does not exist (not an error).
    pub fn load(project: &str) -> Result<Self> {
        let path = rules_path(project);
        if !path.exists() {
            return Ok(PromptRules {
                project: project.to_string(),
                active: vec![],
                stale: vec![],
            });
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {path:?}"))?;
        Self::parse(&text, project)
    }

    /// Write rules.md atomically via a .tmp file.
    pub fn save(&self) -> Result<()> {
        let path = rules_path(&self.project);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create dir {parent:?}"))?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_markdown()).with_context(|| format!("write {tmp:?}"))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Proposal file parser
// ---------------------------------------------------------------------------

/// Parse a proposal .md file and return only the ticked `[x]` rules.
pub fn parse_proposal_file(text: &str) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        // Match ticked checkbox followed by PATTERN
        if (line.starts_with("- [x]") || line.starts_with("- [X]")) && line.contains("PATTERN:") {
            if let Some(rule) = parse_proposal_rule_block(&lines, i) {
                rules.push(rule);
                // Skip forward past this block
                i += 1;
                // skip any continuation lines (EXPANSION, confidence)
                while i < lines.len() {
                    let l = lines[i].trim();
                    if l.starts_with("EXPANSION:")
                        || l.starts_with("confidence:")
                        || l.starts_with("evidence:")
                    {
                        i += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
        }
        i += 1;
    }
    Ok(rules)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse all rule blocks from a named section (e.g. "## Active") in the text.
fn parse_rules_section(text: &str, section_header: &str) -> Vec<Rule> {
    // Find the start of this section.
    let start = match text.find(section_header) {
        Some(pos) => pos + section_header.len(),
        None => return vec![],
    };
    // Find the next `##` heading (or end of file).
    let section_text = &text[start..];
    let end = find_next_section(section_text);
    let section_text = &section_text[..end];

    parse_rule_blocks(section_text)
}

/// Find the byte offset of the next `## ` heading in `s`, or `s.len()` if none.
fn find_next_section(s: &str) -> usize {
    let mut pos = 0;
    for line in s.lines() {
        if pos > 0 && line.starts_with("## ") {
            return pos;
        }
        pos += line.len() + 1; // +1 for newline
    }
    s.len()
}

/// Parse consecutive `- PATTERN:` blocks from a section of text.
fn parse_rule_blocks(section: &str) -> Vec<Rule> {
    let mut rules = Vec::new();
    let lines: Vec<&str> = section.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("- PATTERN:") {
            if let Some(rule) = parse_rule_block_from_lines(&lines, i) {
                rules.push(rule);
                // Advance past the 3 lines of the block
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    rules
}

/// Parse one rule block starting at line index `start` (the `- PATTERN:` line).
/// Returns None if the block is malformed.
fn parse_rule_block_from_lines(lines: &[&str], start: usize) -> Option<Rule> {
    let pattern_line = lines.get(start)?.trim();
    // Strip the `- ` bullet prefix before PATTERN:
    let pattern_part = pattern_line
        .trim_start_matches("- ")
        .trim_start_matches('-')
        .trim();
    let pattern = extract_quoted(pattern_part, "PATTERN:")?;

    let expansion_line = lines.get(start + 1)?.trim();
    let expansion = extract_quoted(expansion_line, "EXPANSION:")?;

    let meta_line = lines.get(start + 2)?.trim();
    let (confidence, added, added_by, last_fired, hits) = parse_meta_line(meta_line)?;

    Some(Rule {
        pattern,
        expansion,
        confidence,
        added,
        added_by,
        last_fired,
        hits,
    })
}

/// Parse a proposal rule block: `- [x] PATTERN: "..."` on the checkbox line,
/// then EXPANSION and confidence on subsequent indented lines.
fn parse_proposal_rule_block(lines: &[&str], start: usize) -> Option<Rule> {
    let pattern_line = lines[start].trim();
    // Strip the `- [x] ` prefix before PATTERN:
    let pattern_part = pattern_line
        .trim_start_matches("- [x]")
        .trim_start_matches("- [X]")
        .trim();
    let pattern = extract_quoted(pattern_part, "PATTERN:")?;

    // Next non-empty line should be EXPANSION
    let expansion_line = lines.get(start + 1)?.trim();
    let expansion = extract_quoted(expansion_line, "EXPANSION:")?;

    // Optional confidence line
    let confidence = if let Some(conf_line) = lines.get(start + 2) {
        let cl = conf_line.trim();
        if cl.starts_with("confidence:") {
            let s = cl.trim_start_matches("confidence:").trim();
            Confidence::parse(s)
        } else {
            Confidence::Med
        }
    } else {
        Confidence::Med
    };

    // Use today's date as default
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    Some(Rule {
        pattern,
        expansion,
        confidence,
        added: today.clone(),
        added_by: String::new(),
        last_fired: None,
        hits: 0,
    })
}

/// Extract a quoted string value from a line like `PATTERN: "foo"`.
fn extract_quoted(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?.trim();
    // May or may not be quoted
    if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
        Some(rest[1..rest.len() - 1].to_string())
    } else {
        // Un-quoted: return as-is
        Some(rest.to_string())
    }
}

/// Parse the meta line:
/// `confidence: high  added: 2026-05-23 by predinvest  last-fired: 2026-05-23  hits: 4`
fn parse_meta_line(line: &str) -> Option<(Confidence, String, String, Option<String>, u32)> {
    // confidence
    let confidence = if let Some(rest) = line.strip_prefix("confidence:") {
        let conf_str: &str = rest.trim().split_whitespace().next().unwrap_or("med");
        Confidence::parse(conf_str)
    } else {
        Confidence::Med
    };

    // added: YYYY-MM-DD by <name>
    let added = extract_field(line, "added:")?;
    // added field looks like "2026-05-23 by predinvest"
    let (added_date, added_by) = if let Some(idx) = added.find(" by ") {
        (
            added[..idx].trim().to_string(),
            added[idx + 4..].trim().to_string(),
        )
    } else {
        (
            added.split_whitespace().next().unwrap_or("").to_string(),
            String::new(),
        )
    };

    // last-fired: YYYY-MM-DD  (or "never")
    let last_fired = extract_field(line, "last-fired:")
        .map(|v| {
            let v = v.trim().split_whitespace().next().unwrap_or("").to_string();
            if v == "never" { None } else { Some(v) }
        })
        .unwrap_or(None);

    // hits: N
    let hits: u32 = extract_field(line, "hits:")
        .and_then(|v| {
            v.trim()
                .split_whitespace()
                .next()
                .map(|s| s.parse().unwrap_or(0))
        })
        .unwrap_or(0);

    Some((confidence, added_date, added_by, last_fired, hits))
}

/// Extract the value of a key from a line with multiple `key: value` pairs.
/// Returns everything after `key:` up to the next `  ` (double space) or end.
fn extract_field<'a>(line: &'a str, key: &str) -> Option<String> {
    let pos = line.find(key)?;
    let after_key = &line[pos + key.len()..];
    // The value ends at the next two-space gap (field separator) or EOL
    let value = if let Some(sep) = after_key.find("  ") {
        after_key[..sep].trim()
    } else {
        after_key.trim()
    };
    Some(value.to_string())
}
