use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BD_LIST_TIMEOUT: Duration = Duration::from_millis(900);
const MAX_ISSUES: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeadsSource {
    Jsonl,
    BdList,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadsView {
    pub repo_path: PathBuf,
    pub source: BeadsSource,
    pub issues: Vec<BeadIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBeadsView {
    pub repos: Vec<BeadsView>,
    pub repo_by_surface_ref: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: Option<i64>,
    pub issue_type: Option<String>,
    pub assignee: Option<String>,
    pub labels: Vec<String>,
    pub updated_at: Option<String>,
}

impl BeadIssue {
    pub fn is_closed(&self) -> bool {
        matches!(
            self.status.as_str(),
            "closed" | "done" | "resolved" | "cancelled"
        )
    }

    pub fn priority_label(&self) -> String {
        self.priority
            .map(|p| format!("P{p}"))
            .unwrap_or_else(|| "P?".to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RawIssue {
    #[serde(default, rename = "_type")]
    record_type: Option<String>,
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    issue_type: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

impl RawIssue {
    fn into_issue(self) -> Option<BeadIssue> {
        if self
            .record_type
            .as_deref()
            .is_some_and(|record_type| record_type != "issue")
        {
            return None;
        }
        if self.id.trim().is_empty() || self.title.trim().is_empty() {
            return None;
        }
        Some(BeadIssue {
            id: self.id,
            title: self.title,
            status: if self.status.trim().is_empty() {
                "open".to_string()
            } else {
                self.status
            },
            priority: self.priority,
            issue_type: self.issue_type,
            assignee: self.assignee,
            labels: self.labels,
            updated_at: self.updated_at,
        })
    }
}

pub fn load_for_repo_path(repo: &Path) -> BeadsView {
    // Prefer live `bd list`, but only when it actually returns issues. The local
    // bd database can lag the committed `issues.jsonl` export — e.g. a fresh
    // clone, or a teammate's issues not yet `bd import`ed here — so `bd list`
    // reports 0 while the jsonl still holds the open work the user sees. Fall
    // back to the jsonl in that case rather than claiming "no active beads".
    if let Some(mut issues) = load_bd_list(&repo) {
        if !issues.is_empty() {
            sort_and_cap(&mut issues);
            return BeadsView {
                repo_path: repo.to_path_buf(),
                source: BeadsSource::BdList,
                issues,
            };
        }
    }

    if let Some(mut issues) = load_jsonl(&repo) {
        if !issues.is_empty() {
            sort_and_cap(&mut issues);
            return BeadsView {
                repo_path: repo.to_path_buf(),
                source: BeadsSource::Jsonl,
                issues,
            };
        }
    }

    BeadsView {
        repo_path: repo.to_path_buf(),
        source: BeadsSource::Unavailable,
        issues: Vec::new(),
    }
}

pub fn workspace_view_for_repos(
    ordered_repo_roots: &[PathBuf],
    repo_by_surface_ref: HashMap<String, PathBuf>,
) -> Option<WorkspaceBeadsView> {
    let mut seen = HashSet::new();
    let mut repos = Vec::new();
    for repo in ordered_repo_roots {
        let repo = repo.clone();
        if !seen.insert(repo.clone()) {
            continue;
        }
        if repo.join(".beads").is_dir() {
            repos.push(load_for_repo_path(&repo));
        }
    }
    if repos.is_empty() {
        None
    } else {
        Some(WorkspaceBeadsView {
            repos,
            repo_by_surface_ref,
        })
    }
}

fn load_jsonl(repo: &Path) -> Option<Vec<BeadIssue>> {
    let path = repo.join(".beads").join("issues.jsonl");
    let raw = std::fs::read_to_string(path).ok()?;
    let issues: Vec<BeadIssue> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<RawIssue>(line).ok())
        .filter_map(RawIssue::into_issue)
        .collect();
    Some(issues)
}

fn load_bd_list(repo: &Path) -> Option<Vec<BeadIssue>> {
    let mut child = Command::new("bd")
        .args(["list", "--json"])
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        if start.elapsed() > BD_LIST_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let raw: Vec<RawIssue> = serde_json::from_slice(&output.stdout).ok()?;
    Some(raw.into_iter().filter_map(RawIssue::into_issue).collect())
}

fn sort_and_cap(issues: &mut Vec<BeadIssue>) {
    issues.sort_by(|a, b| {
        let a_key = issue_sort_key(a);
        let b_key = issue_sort_key(b);
        a_key
            .cmp(&b_key)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.title.cmp(&b.title))
    });
    issues.truncate(MAX_ISSUES);
}

fn issue_sort_key(issue: &BeadIssue) -> (u8, i64, u8) {
    (
        if issue.is_closed() { 1 } else { 0 },
        issue.priority.unwrap_or(9),
        status_rank(&issue.status),
    )
}

fn status_rank(status: &str) -> u8 {
    match status {
        "in_progress" | "active" => 0,
        "open" | "todo" => 1,
        "blocked" => 2,
        "closed" | "done" | "resolved" => 9,
        _ => 5,
    }
}

pub fn compact_issue_title(title: &str, max_chars: usize) -> String {
    let normalized = title.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&normalized, max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_export_jsonl_records() {
        let tmp = tempfile::tempdir().unwrap();
        let beads = tmp.path().join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        std::fs::write(
            beads.join("issues.jsonl"),
            r#"{"_type":"issue","id":"repo-1","title":"First","status":"in_progress","priority":1,"labels":["a"]}"#,
        )
        .unwrap();

        let view = load_for_repo_path(tmp.path());
        assert_eq!(view.source, BeadsSource::Jsonl);
        assert_eq!(view.issues.len(), 1);
        assert_eq!(view.issues[0].id, "repo-1");
        assert_eq!(view.issues[0].priority_label(), "P1");
    }

    #[test]
    fn sorts_active_high_priority_before_closed() {
        let mut issues = vec![
            BeadIssue {
                id: "c".to_string(),
                title: "closed".to_string(),
                status: "closed".to_string(),
                priority: Some(0),
                issue_type: None,
                assignee: None,
                labels: vec![],
                updated_at: None,
            },
            BeadIssue {
                id: "b".to_string(),
                title: "p2".to_string(),
                status: "open".to_string(),
                priority: Some(2),
                issue_type: None,
                assignee: None,
                labels: vec![],
                updated_at: None,
            },
            BeadIssue {
                id: "a".to_string(),
                title: "p1".to_string(),
                status: "in_progress".to_string(),
                priority: Some(1),
                issue_type: None,
                assignee: None,
                labels: vec![],
                updated_at: None,
            },
        ];
        sort_and_cap(&mut issues);
        assert_eq!(
            issues.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn sorts_recent_updates_first_within_same_bucket() {
        let mut issues = vec![
            BeadIssue {
                id: "old".to_string(),
                title: "Old".to_string(),
                status: "open".to_string(),
                priority: Some(2),
                issue_type: None,
                assignee: None,
                labels: vec![],
                updated_at: Some("2026-06-04T18:00:00Z".to_string()),
            },
            BeadIssue {
                id: "new".to_string(),
                title: "New".to_string(),
                status: "open".to_string(),
                priority: Some(2),
                issue_type: None,
                assignee: None,
                labels: vec![],
                updated_at: Some("2026-06-04T19:00:00Z".to_string()),
            },
        ];
        sort_and_cap(&mut issues);
        assert_eq!(
            issues.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["new", "old"]
        );
    }
}
