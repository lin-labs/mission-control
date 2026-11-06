//! View model for the Updates screen: session records -> sorted, grouped rows.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use super::artifacts::Artifact;
use super::record::{SegState, Segment, SessionFile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    MyInput,
    AgentResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    Workspace,
    Flat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub group: String,
    pub host: String,
    pub session: String,
    pub segment: String,
    pub intent: String,
    pub latest: String,
    pub at: DateTime<Utc>,
    pub state: SegState,
    pub dim: bool,
    pub artifacts: Vec<Artifact>,
    pub entries: Vec<Entry>,
    pub workspace: Option<String>,
    pub surface: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Input {
        at: DateTime<Utc>,
        text: String,
        replies: usize,
    },
    /// Injected (loop) turn only; plain replies are counted in `Input::replies`.
    Turn { at: DateTime<Utc>, injected: bool },
}

const MAX_AGE_DAYS: i64 = 7;

/// Rows come out group by group (best row's group first, unattached last),
/// newest `at` first within a group.
pub fn build_rows(
    files: &[SessionFile],
    order: Order,
    grouping: Grouping,
    now: DateTime<Utc>,
    workspace_names: &HashMap<String, String>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = files
        .iter()
        .flat_map(|f| {
            f.segments
                .iter()
                .map(move |s| row(f, s, order, grouping, workspace_names))
        })
        .filter(|r| now - r.at <= Duration::days(MAX_AGE_DAYS))
        .collect();
    rows.sort_by(|a, b| b.at.cmp(&a.at));
    let mut rank: HashMap<String, usize> = HashMap::new();
    for r in &rows {
        let n = rank.len();
        rank.entry(r.group.clone()).or_insert(n);
    }
    let unattached = |g: &str| grouping == Grouping::Workspace && g.ends_with(UNATTACHED);
    rows.sort_by_key(|r| (unattached(&r.group), rank[&r.group]));
    rows
}

const UNATTACHED: &str = " · unattached";

fn row(
    f: &SessionFile,
    s: &Segment,
    order: Order,
    grouping: Grouping,
    names: &HashMap<String, String>,
) -> Row {
    let last_input = s.user_inputs.iter().map(|u| u.at).max();
    let at = match order {
        Order::MyInput => last_input.or(s.latest_at),
        Order::AgentResponse => s
            .agent_turns
            .iter()
            .map(|t| t.at)
            .max()
            .or(s.latest_at)
            .or(last_input),
    }
    .unwrap_or(f.updated_at);
    let group = match (grouping, &f.workspace) {
        (Grouping::Flat, _) => "All".to_string(),
        (_, Some(id)) => names.get(id).cloned().unwrap_or_else(|| id.clone()),
        (_, None) => format!("{}{UNATTACHED}", f.host),
    };
    Row {
        group,
        host: f.host.clone(),
        session: f.session.clone(),
        segment: s.segment.clone(),
        intent: s.intent.clone(),
        latest: s.latest.clone(),
        at,
        state: s.state,
        dim: s.state == SegState::Done,
        artifacts: s.artifacts.clone(),
        entries: entries(s),
        workspace: f.workspace.clone(),
        surface: f.surface.clone(),
    }
}

fn entries(s: &Segment) -> Vec<Entry> {
    let mut inputs: Vec<_> = s.user_inputs.iter().collect();
    inputs.sort_by_key(|u| u.at);
    let mut out: Vec<Entry> = inputs
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let next = inputs.get(i + 1).map(|n| n.at);
            let replies = s
                .agent_turns
                .iter()
                .filter(|t| !t.injected && t.at >= u.at && next.is_none_or(|n| t.at < n))
                .count();
            Entry::Input {
                at: u.at,
                text: u.text.clone(),
                replies,
            }
        })
        .collect();
    out.extend(
        s.agent_turns
            .iter()
            .filter(|t| t.injected)
            .map(|t| Entry::Turn {
                at: t.at,
                injected: true,
            }),
    );
    // Stable: on equal times an input stays before the turn.
    out.sort_by_key(|e| match e {
        Entry::Input { at, .. } | Entry::Turn { at, .. } => *at,
    });
    out
}
