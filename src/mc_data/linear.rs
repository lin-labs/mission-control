use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use tokio::process::Command;

const LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";
const GCP_PROJECT: &str = "reflectionai";
const GCP_SECRET: &str = "LINEAR_API_KEY";
const GCP_SECRET_TIMEOUT: Duration = Duration::from_secs(8);
const API_TIMEOUT: Duration = Duration::from_secs(12);
const QUERY_LIMIT: i64 = 100;
const MAX_ISSUES: usize = 12;

const ISSUES_QUERY: &str = r#"
query MissionControlIssues($projectId: String!, $first: Int!, $filter: IssueFilter) {
  project(id: $projectId) {
    issues(first: $first, filter: $filter) {
      nodes {
        identifier
        title
        priority
        updatedAt
        state { name type }
        labels { nodes { name } }
        url
      }
    }
  }
}
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearApiKeyResolution {
    pub api_key: Option<String>,
    pub warning: Option<String>,
}

/// Resolve the Linear API key without persisting or logging it. An explicit
/// environment value wins; otherwise use the machine's gcloud authentication.
pub async fn resolve_api_key() -> LinearApiKeyResolution {
    let explicit = std::env::var("LINEAR_API_KEY").ok();
    let args = [
        "secrets",
        "versions",
        "access",
        "latest",
        "--secret",
        GCP_SECRET,
        "--project",
        GCP_PROJECT,
    ];
    resolve_api_key_with_command(explicit, "gcloud", &args, GCP_SECRET_TIMEOUT).await
}

async fn resolve_api_key_with_command(
    explicit: Option<String>,
    binary: &str,
    args: &[&str],
    timeout: Duration,
) -> LinearApiKeyResolution {
    if let Some(key) = explicit
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        return LinearApiKeyResolution {
            api_key: Some(key),
            warning: None,
        };
    }

    let mut command = Command::new(binary);
    command.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, command.output()).await {
        Err(_) => LinearApiKeyResolution {
            api_key: None,
            warning: Some(
                "Linear unavailable: gcloud secret lookup timed out; authenticate gcloud and reload mc"
                    .to_string(),
            ),
        },
        Ok(Err(_)) => LinearApiKeyResolution {
            api_key: None,
            warning: Some(
                "Linear unavailable: could not run gcloud; install/authenticate gcloud and reload mc"
                    .to_string(),
            ),
        },
        Ok(Ok(output)) if !output.status.success() => LinearApiKeyResolution {
            api_key: None,
            warning: Some(format!(
                "Linear unavailable: gcloud secret lookup failed (exit {}); authenticate gcloud and reload mc",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            )),
        },
        Ok(Ok(output)) => {
            let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if key.is_empty() {
                LinearApiKeyResolution {
                    api_key: None,
                    warning: Some(
                        "Linear unavailable: gcloud secret lookup returned an empty value; reload mc after fixing access"
                            .to_string(),
                    ),
                }
            } else {
                LinearApiKeyResolution {
                    api_key: Some(key),
                    warning: None,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearIssue {
    pub identifier: String,
    pub title: String,
    pub priority: i64,
    pub updated_at: Option<String>,
    pub state_name: String,
    pub state_type: String,
    pub labels: Vec<String>,
    pub url: Option<String>,
}

/// Workspace-owned projection state. Keeping the last successful `issues`
/// alongside an optional warning lets the UI retain useful data when a later
/// refresh fails without ever retaining credentials or raw API responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLinearView {
    pub project_id: String,
    pub required_labels: Vec<String>,
    pub issues: Vec<LinearIssue>,
    pub warning: Option<String>,
}

impl WorkspaceLinearView {
    pub fn issue(&self, identifier: &str) -> Option<&LinearIssue> {
        self.issues
            .iter()
            .find(|issue| issue.identifier == identifier)
    }

    pub fn desktop_url_for_issue(&self, identifier: &str) -> Option<String> {
        self.issue(identifier).and_then(LinearIssue::desktop_url)
    }
}

impl LinearIssue {
    pub fn priority_label(&self) -> &'static str {
        match self.priority {
            1 => "P0",
            2 => "P1",
            3 => "P2",
            4 => "P3",
            _ => "P?",
        }
    }

    pub fn is_started(&self) -> bool {
        self.state_type == "started"
    }

    pub fn desktop_url(&self) -> Option<String> {
        self.url.as_deref().and_then(linear_desktop_url)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearClientError {
    InvalidTarget,
    TimedOut,
    RequestFailed,
    HttpStatus(u16),
    ApiRejected,
    InvalidResponse,
    ProjectNotFound,
}

impl fmt::Display for LinearClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidTarget => "Linear unavailable: project configuration is invalid",
            Self::TimedOut => "Linear unavailable: API request timed out",
            Self::RequestFailed => "Linear unavailable: API request failed",
            Self::HttpStatus(_) => "Linear unavailable: API request was rejected",
            Self::ApiRejected => "Linear unavailable: API returned an error",
            Self::InvalidResponse => "Linear unavailable: API response was invalid",
            Self::ProjectNotFound => "Linear unavailable: configured project was not found",
        };
        match self {
            Self::HttpStatus(status) => write!(f, "{message} (HTTP {status})"),
            _ => f.write_str(message),
        }
    }
}

impl std::error::Error for LinearClientError {}

#[derive(Clone)]
pub struct LinearClient {
    http: Client,
    api_key: String,
}

impl LinearClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
        }
    }

    /// Fetch a bounded projection of active issues for one Linear project.
    /// Label matching is conjunctive: every configured label must be present.
    pub async fn fetch_issues(
        &self,
        project_id: &str,
        required_labels: &[String],
    ) -> Result<Vec<LinearIssue>, LinearClientError> {
        if project_id.trim().is_empty() {
            return Err(LinearClientError::InvalidTarget);
        }

        match tokio::time::timeout(
            API_TIMEOUT,
            self.fetch_issues_inner(project_id, required_labels),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(LinearClientError::TimedOut),
        }
    }

    async fn fetch_issues_inner(
        &self,
        project_id: &str,
        required_labels: &[String],
    ) -> Result<Vec<LinearIssue>, LinearClientError> {
        let body = GraphQlRequest {
            query: ISSUES_QUERY,
            variables: QueryVariables {
                project_id,
                first: QUERY_LIMIT,
                filter: required_label_filter(required_labels),
            },
        };
        let response = self
            .http
            .post(LINEAR_ENDPOINT)
            .header("Authorization", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| LinearClientError::RequestFailed)?;
        if !response.status().is_success() {
            return Err(LinearClientError::HttpStatus(response.status().as_u16()));
        }
        let response = response
            .json::<GraphQlResponse>()
            .await
            .map_err(|_| LinearClientError::InvalidResponse)?;
        project_issues(response, required_labels)
    }
}

#[derive(Serialize)]
struct GraphQlRequest<'a> {
    query: &'static str,
    variables: QueryVariables<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QueryVariables<'a> {
    project_id: &'a str,
    first: i64,
    filter: Option<IssueFilter<'a>>,
}

#[derive(Serialize)]
struct IssueFilter<'a> {
    and: Vec<LabelRequirement<'a>>,
}

#[derive(Serialize)]
struct LabelRequirement<'a> {
    labels: LabelCollectionFilter<'a>,
}

#[derive(Serialize)]
struct LabelCollectionFilter<'a> {
    some: LabelFilter<'a>,
}

#[derive(Serialize)]
struct LabelFilter<'a> {
    name: StringComparator<'a>,
}

#[derive(Serialize)]
struct StringComparator<'a> {
    eq: &'a str,
}

fn required_label_filter(required_labels: &[String]) -> Option<IssueFilter<'_>> {
    let and = required_labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .map(|label| LabelRequirement {
            labels: LabelCollectionFilter {
                some: LabelFilter {
                    name: StringComparator { eq: label },
                },
            },
        })
        .collect::<Vec<_>>();
    (!and.is_empty()).then_some(IssueFilter { and })
}

#[derive(Deserialize)]
struct GraphQlResponse {
    data: Option<ResponseData>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct ResponseData {
    project: Option<RawProject>,
}

#[derive(Deserialize)]
struct RawProject {
    issues: RawIssueConnection,
}

#[derive(Deserialize)]
struct RawIssueConnection {
    #[serde(default)]
    nodes: Vec<RawIssue>,
}

#[derive(Deserialize)]
struct RawIssue {
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    priority: i64,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<String>,
    state: Option<RawState>,
    #[serde(default)]
    labels: RawLabelConnection,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct RawState {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Default, Deserialize)]
struct RawLabelConnection {
    #[serde(default)]
    nodes: Vec<RawLabel>,
}

#[derive(Deserialize)]
struct RawLabel {
    #[serde(default)]
    name: String,
}

fn project_issues(
    response: GraphQlResponse,
    required_labels: &[String],
) -> Result<Vec<LinearIssue>, LinearClientError> {
    if !response.errors.is_empty() {
        return Err(LinearClientError::ApiRejected);
    }
    let data = response.data.ok_or(LinearClientError::InvalidResponse)?;
    let project = data.project.ok_or(LinearClientError::ProjectNotFound)?;
    let required_labels: Vec<&str> = required_labels
        .iter()
        .map(|label| label.trim())
        .filter(|label| !label.is_empty())
        .collect();

    let mut issues: Vec<LinearIssue> = project
        .issues
        .nodes
        .into_iter()
        .filter_map(|issue| issue.into_issue())
        .filter(|issue| is_active_state(&issue.state_type))
        .filter(|issue| {
            required_labels
                .iter()
                .all(|required| issue.labels.iter().any(|actual| actual == required))
        })
        .collect();
    sort_and_cap(&mut issues);
    Ok(issues)
}

impl RawIssue {
    fn into_issue(self) -> Option<LinearIssue> {
        let state = self.state?;
        if self.identifier.trim().is_empty() || self.title.trim().is_empty() {
            return None;
        }
        Some(LinearIssue {
            identifier: self.identifier,
            title: self.title,
            priority: self.priority,
            updated_at: self.updated_at,
            state_name: state.name,
            state_type: state.kind,
            labels: self
                .labels
                .nodes
                .into_iter()
                .map(|label| label.name)
                .filter(|name| !name.trim().is_empty())
                .collect(),
            url: self.url,
        })
    }
}

fn is_active_state(state_type: &str) -> bool {
    !matches!(state_type, "completed" | "canceled" | "cancelled")
}

fn sort_and_cap(issues: &mut Vec<LinearIssue>) {
    issues.sort_by(|a, b| {
        issue_sort_key(a)
            .cmp(&issue_sort_key(b))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.identifier.cmp(&b.identifier))
            .then_with(|| a.title.cmp(&b.title))
    });
    issues.truncate(MAX_ISSUES);
}

fn issue_sort_key(issue: &LinearIssue) -> (u8, u8) {
    (
        if issue.is_started() { 0 } else { 1 },
        match issue.priority {
            1 => 0,
            2 => 1,
            3 => 2,
            4 => 3,
            _ => 9,
        },
    )
}

/// Convert a trusted Linear issue URL into the desktop app protocol. URLs for
/// other hosts, non-HTTPS URLs, and non-issue paths are rejected.
pub fn linear_desktop_url(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("linear.app")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    let issue_segment = segments.iter().position(|segment| *segment == "issue")?;
    if segments
        .get(issue_segment + 1)
        .is_none_or(|identifier| identifier.trim().is_empty())
    {
        return None;
    }
    // `url::Url::set_scheme` rejects switching from a WHATWG "special"
    // scheme (https) to the app's custom scheme, so reconstruct only from the
    // already-validated origin and parsed path/query/fragment components.
    let mut desktop = format!("linear://linear.app{}", url.path());
    if let Some(query) = url.query() {
        desktop.push('?');
        desktop.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        desktop.push('#');
        desktop.push_str(fragment);
    }
    Some(desktop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response_with_issues(issues: serde_json::Value) -> GraphQlResponse {
        serde_json::from_value(json!({
            "data": { "project": { "issues": { "nodes": issues } } }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn explicit_key_skips_external_lookup() {
        let result = resolve_api_key_with_command(
            Some("  explicit-key\n".into()),
            "/definitely/missing/gcloud",
            &[],
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result.api_key.as_deref(), Some("explicit-key"));
        assert_eq!(result.warning, None);
    }

    #[tokio::test]
    async fn successful_lookup_trims_key() {
        let result = resolve_api_key_with_command(
            None,
            "/bin/sh",
            &["-c", "printf 'secret-from-gcloud\\n'"],
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.api_key.as_deref(), Some("secret-from-gcloud"));
        assert_eq!(result.warning, None);
    }

    #[tokio::test]
    async fn failed_lookup_is_sanitized() {
        let result = resolve_api_key_with_command(
            None,
            "/bin/sh",
            &["-c", "printf 'sensitive stderr' >&2; exit 7"],
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.api_key, None);
        let warning = result.warning.unwrap();
        assert!(warning.contains("exit 7"));
        assert!(!warning.contains("sensitive"));
    }

    #[tokio::test]
    async fn timed_out_lookup_is_sanitized() {
        let result = resolve_api_key_with_command(
            None,
            "/bin/sh",
            &["-c", "sleep 1"],
            Duration::from_millis(5),
        )
        .await;
        assert_eq!(result.api_key, None);
        assert!(result.warning.unwrap().contains("timed out"));
    }

    #[test]
    fn filters_active_issues_by_all_labels_and_sorts_started_first() {
        let response = response_with_issues(json!([
            {
                "identifier": "MID-1", "title": "Urgent backlog", "priority": 1,
                "updatedAt": "2026-07-14T03:00:00Z",
                "state": { "name": "Backlog", "type": "backlog" },
                "labels": { "nodes": [{ "name": "group-grader" }, { "name": "mac" }] },
                "url": "https://linear.app/acme/issue/MID-1/urgent-backlog"
            },
            {
                "identifier": "MID-2", "title": "Started low", "priority": 4,
                "updatedAt": "2026-07-14T02:00:00Z",
                "state": { "name": "In Progress", "type": "started" },
                "labels": { "nodes": [{ "name": "group-grader" }, { "name": "mac" }] },
                "url": "https://linear.app/acme/issue/MID-2/started-low"
            },
            {
                "identifier": "MID-3", "title": "Missing a label", "priority": 2,
                "state": { "name": "In Progress", "type": "started" },
                "labels": { "nodes": [{ "name": "group-grader" }] },
                "url": "https://linear.app/acme/issue/MID-3/missing-label"
            },
            {
                "identifier": "MID-4", "title": "Already done", "priority": 1,
                "state": { "name": "Done", "type": "completed" },
                "labels": { "nodes": [{ "name": "group-grader" }, { "name": "mac" }] },
                "url": "https://linear.app/acme/issue/MID-4/done"
            }
        ]));

        let issues = project_issues(response, &["group-grader".into(), "mac".into()]).unwrap();
        assert_eq!(
            issues
                .iter()
                .map(|issue| issue.identifier.as_str())
                .collect::<Vec<_>>(),
            vec!["MID-2", "MID-1"]
        );
        assert_eq!(issues[0].priority_label(), "P3");
        assert_eq!(issues[1].priority_label(), "P0");
    }

    #[test]
    fn query_filter_requires_every_configured_label_at_the_api() {
        let labels = vec![" group-grader ".to_string(), "mac".to_string()];
        let variables = QueryVariables {
            project_id: "project-1",
            first: QUERY_LIMIT,
            filter: required_label_filter(&labels),
        };
        assert_eq!(
            serde_json::to_value(variables).unwrap(),
            json!({
                "projectId": "project-1",
                "first": QUERY_LIMIT,
                "filter": {
                    "and": [
                        { "labels": { "some": { "name": { "eq": "group-grader" } } } },
                        { "labels": { "some": { "name": { "eq": "mac" } } } }
                    ]
                }
            })
        );
        assert!(required_label_filter(&[]).is_none());
    }

    #[test]
    fn sorts_priorities_and_recent_updates_then_caps_at_twelve() {
        let issues: Vec<serde_json::Value> = (0..15)
            .map(|index| {
                json!({
                    "identifier": format!("MID-{index}"),
                    "title": format!("Issue {index}"),
                    "priority": if index == 14 { 1 } else { 3 },
                    "updatedAt": format!("2026-07-14T{:02}:00:00Z", index),
                    "state": { "name": "Todo", "type": "unstarted" },
                    "labels": { "nodes": [] },
                    "url": format!("https://linear.app/acme/issue/MID-{index}/issue")
                })
            })
            .collect();
        let projected = project_issues(response_with_issues(json!(issues)), &[]).unwrap();
        assert_eq!(projected.len(), MAX_ISSUES);
        assert_eq!(projected[0].identifier, "MID-14");
        assert_eq!(projected[1].identifier, "MID-13");
    }

    #[test]
    fn maps_linear_priorities_to_mission_control_labels() {
        for (linear, expected) in [(1, "P0"), (2, "P1"), (3, "P2"), (4, "P3"), (0, "P?")] {
            let issue = LinearIssue {
                identifier: "MID-1".into(),
                title: "Issue".into(),
                priority: linear,
                updated_at: None,
                state_name: "Todo".into(),
                state_type: "unstarted".into(),
                labels: vec![],
                url: None,
            };
            assert_eq!(issue.priority_label(), expected);
        }
    }

    #[test]
    fn graphql_errors_are_sanitized() {
        let response: GraphQlResponse = serde_json::from_value(json!({
            "data": null,
            "errors": [{ "message": "secret response detail" }]
        }))
        .unwrap();
        let error = project_issues(response, &[]).unwrap_err();
        assert_eq!(error, LinearClientError::ApiRejected);
        assert!(!error.to_string().contains("secret response detail"));
    }

    #[test]
    fn converts_only_trusted_linear_issue_urls() {
        assert_eq!(
            linear_desktop_url("https://linear.app/acme/issue/MID-123/fix-it?foo=bar"),
            Some("linear://linear.app/acme/issue/MID-123/fix-it?foo=bar".into())
        );
        assert_eq!(
            linear_desktop_url("http://linear.app/acme/issue/MID-1/x"),
            None
        );
        assert_eq!(
            linear_desktop_url("https://linear.app.evil/acme/issue/MID-1/x"),
            None
        );
        assert_eq!(linear_desktop_url("https://linear.app/acme/settings"), None);
        assert_eq!(linear_desktop_url("not a URL"), None);
    }

    #[test]
    fn issue_without_url_has_no_desktop_target() {
        let issue = LinearIssue {
            identifier: "MID-1".into(),
            title: "Issue".into(),
            priority: 0,
            updated_at: None,
            state_name: "Todo".into(),
            state_type: "unstarted".into(),
            labels: vec![],
            url: None,
        };
        assert_eq!(issue.desktop_url(), None);
    }

    #[test]
    fn workspace_view_maps_identifier_to_exact_desktop_url() {
        let view = WorkspaceLinearView {
            project_id: "project-id".into(),
            required_labels: vec!["group-grader".into()],
            issues: vec![LinearIssue {
                identifier: "MID-508".into(),
                title: "Exact issue".into(),
                priority: 2,
                updated_at: None,
                state_name: "Todo".into(),
                state_type: "unstarted".into(),
                labels: vec!["group-grader".into()],
                url: Some("https://linear.app/acme/issue/MID-508/exact-issue".into()),
            }],
            warning: Some("Linear unavailable: last refresh failed".into()),
        };

        assert_eq!(
            view.desktop_url_for_issue("MID-508"),
            Some("linear://linear.app/acme/issue/MID-508/exact-issue".into())
        );
        assert_eq!(view.desktop_url_for_issue("MID-999"), None);
        assert_eq!(view.issues.len(), 1, "last-good rows remain available");
    }
}
