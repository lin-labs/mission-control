use std::collections::HashMap;

use chrono::{DateTime, Duration, TimeZone, Utc};
use mission_control::mc_data::updates::record::*;
use mission_control::mc_data::updates::view::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 10, 12, 0, 0).unwrap()
}

fn ago(h: i64, m: i64) -> DateTime<Utc> {
    now() - Duration::hours(h) - Duration::minutes(m)
}

fn seg(name: &str, inputs: &[DateTime<Utc>], turns: &[(DateTime<Utc>, bool)]) -> Segment {
    Segment {
        segment: name.into(),
        intent: format!("intent {name}"),
        latest: String::new(),
        latest_at: None,
        state: SegState::Working,
        needs_me: None,
        user_inputs: inputs
            .iter()
            .map(|&at| UserInput {
                at,
                text: "t".into(),
            })
            .collect(),
        agent_turns: turns
            .iter()
            .map(|&(at, injected)| AgentTurn { at, injected })
            .collect(),
        artifacts: vec![],
        last_refine_at: None,
    }
}

fn file(session: &str, ws: Option<&str>, segments: Vec<Segment>) -> SessionFile {
    SessionFile {
        host: "mac".into(),
        session: session.into(),
        agent: "claude".into(),
        transcript: "/t.jsonl".into(),
        workspace: ws.map(Into::into),
        surface: None,
        status: None,
        segments,
        updated_at: now(),
    }
}

fn build(files: &[SessionFile], order: Order, grouping: Grouping) -> Vec<Row> {
    build_rows(files, order, grouping, now(), &HashMap::new())
}

fn names(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|r| r.segment.as_str()).collect()
}

#[test]
fn my_input_orders_by_latest_user_input() {
    let files = [file(
        "s",
        None,
        vec![
            seg("old", &[ago(3, 0)], &[(ago(0, 1), false)]),
            seg("new", &[ago(1, 0)], &[]),
        ],
    )];
    assert_eq!(
        names(&build(&files, Order::MyInput, Grouping::Flat)),
        ["new", "old"]
    );
    assert_eq!(
        names(&build(&files, Order::AgentResponse, Grouping::Flat)),
        ["old", "new"]
    );
}

#[test]
fn agent_order_counts_injected_turns() {
    let files = [file(
        "s",
        None,
        vec![
            seg("a", &[ago(5, 0)], &[(ago(2, 0), false)]),
            seg("b", &[ago(6, 0)], &[(ago(0, 1), true)]),
        ],
    )];
    assert_eq!(
        names(&build(&files, Order::AgentResponse, Grouping::Flat)),
        ["b", "a"]
    );
}

#[test]
fn replies_attach_to_preceding_input() {
    let (t10, t11) = (ago(2, 0), ago(1, 0));
    let m = |n| Duration::minutes(n);
    let files = [file(
        "s",
        None,
        vec![seg(
            "x",
            &[t10, t11],
            &[
                (t10 + m(5), false),
                (t10 + m(50), false),
                (t11 + m(2), false),
                (t11 + m(3), true),
            ],
        )],
    )];
    let rows = build(&files, Order::MyInput, Grouping::Flat);
    let e = &rows[0].entries;
    assert!(matches!(e[0], Entry::Input { replies: 2, .. }));
    assert!(matches!(e[1], Entry::Input { replies: 1, .. }));
    assert_eq!(e.len(), 3);
    assert!(matches!(e[2], Entry::Turn { injected: true, .. }));
}

#[test]
fn grouped_by_workspace_name() {
    let files = [
        file("u", None, vec![seg("loose", &[ago(0, 5)], &[])]),
        file("a", Some("ws1"), vec![seg("named", &[ago(2, 0)], &[])]),
        file("b", Some("ws2"), vec![seg("raw", &[ago(3, 0)], &[])]),
    ];
    let n = HashMap::from([("ws1".to_string(), "Alpha".to_string())]);
    let rows = build_rows(&files, Order::MyInput, Grouping::Workspace, now(), &n);
    let groups: Vec<_> = rows.iter().map(|r| r.group.as_str()).collect();
    assert_eq!(groups, ["Alpha", "ws2", "mac · unattached"]);
}

#[test]
fn flat_has_one_group() {
    let files = [
        file("a", Some("ws1"), vec![seg("x", &[ago(1, 0)], &[])]),
        file("b", None, vec![seg("y", &[ago(2, 0)], &[])]),
    ];
    let rows = build(&files, Order::MyInput, Grouping::Flat);
    assert!(rows.iter().all(|r| r.group == "All"));
    assert_eq!(rows.len(), 2);
}

#[test]
fn older_than_seven_days_hidden() {
    let files = [file(
        "s",
        None,
        vec![
            seg(
                "stale",
                &[now() - Duration::days(7) - Duration::minutes(1)],
                &[],
            ),
            seg("edge", &[now() - Duration::days(7)], &[]),
        ],
    )];
    assert_eq!(
        names(&build(&files, Order::MyInput, Grouping::Flat)),
        ["edge"]
    );
}

#[test]
fn done_is_dim() {
    let mut done = seg("d", &[ago(1, 0)], &[]);
    done.state = SegState::Done;
    let files = [file("s", None, vec![done, seg("w", &[ago(2, 0)], &[])])];
    let rows = build(&files, Order::MyInput, Grouping::Flat);
    assert_eq!((rows[0].dim, rows[1].dim), (true, false));
}
