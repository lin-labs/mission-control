//! Artifacts (PRs, tickets, links, commits, files) mentioned in one transcript line.

use std::collections::HashSet;
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Pr,
    Ticket,
    Notion,
    Slack,
    Commit,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    #[serde(rename = "ref")]
    pub reference: String,
    pub at: DateTime<Utc>,
}

static PR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https://github\.com/([\w.-]+/[\w.-]+)/pull/(\d+)").unwrap());
static TICKET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:MID|BLN)-\d+\b").unwrap());
static NOTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https://(?:[\w-]+\.)*notion\.(?:so|com|site)/[^\s)>\]"'`\\]+"#).unwrap()
});
static SLACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https://[\w-]+\.slack\.com/[^\s)>\]"'`\\]+"#).unwrap());
/// git `[branch abc1234] msg`; jj `Parent commit (@-)  : <change> <commit>`.
static COMMIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\[[\w./-]+(?: \(root-commit\))? ([0-9a-f]{7,40})\]|Parent commit \(@-\)\s*: \S+ ([0-9a-f]{7,40})",
    )
    .unwrap()
});

const FILE_TOOLS: [&str; 3] = ["Write", "Edit", "NotebookEdit"];

/// Artifacts in `line`, deduplicated, in first-seen order.
pub fn from_line(line: &Value, at: DateTime<Utc>) -> Vec<Artifact> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |kind, reference: &str| {
        let reference = reference.trim_end_matches(['.', ',', ';', ':']).to_string();
        let a = Artifact {
            kind,
            reference,
            at,
        };
        if seen.insert(a.clone()) {
            out.push(a);
        }
    };
    let Some(content) = line.pointer("/message/content") else {
        return out;
    };
    let mut texts = Vec::new();
    match content {
        Value::String(s) => texts.push(s.as_str()),
        Value::Array(blocks) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => texts.extend(b.get("text").and_then(Value::as_str)),
                    Some("tool_result") => collect_strings(b.get("content"), &mut texts),
                    Some("tool_use") => {
                        let input = b.get("input");
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                        if FILE_TOOLS.contains(&name) {
                            // Scan only the path: written content may quote unrelated URLs.
                            let path = ["file_path", "notebook_path"]
                                .iter()
                                .find_map(|k| input?.get(k)?.as_str());
                            if let Some(p) = path {
                                add(ArtifactKind::File, p);
                            }
                        } else {
                            collect_strings(input, &mut texts);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    for t in texts {
        for c in PR.captures_iter(t) {
            add(ArtifactKind::Pr, &format!("{}#{}", &c[1], &c[2]));
        }
        for m in TICKET.find_iter(t) {
            add(ArtifactKind::Ticket, m.as_str());
        }
        for m in NOTION.find_iter(t) {
            add(ArtifactKind::Notion, m.as_str());
        }
        for m in SLACK.find_iter(t) {
            add(ArtifactKind::Slack, m.as_str());
        }
        for c in COMMIT.captures_iter(t) {
            add(
                ArtifactKind::Commit,
                c.get(1).or(c.get(2)).unwrap().as_str(),
            );
        }
    }
    out
}

/// Every string in `v`: a string itself, or strings nested in arrays/objects (`text` blocks, tool inputs).
fn collect_strings<'a>(v: Option<&'a Value>, out: &mut Vec<&'a str>) {
    match v {
        Some(Value::String(s)) => out.push(s),
        Some(Value::Array(a)) => a.iter().for_each(|x| collect_strings(Some(x), out)),
        Some(Value::Object(o)) => o.values().for_each(|x| collect_strings(Some(x), out)),
        _ => {}
    }
}
