use chrono::{TimeZone, Utc};
use mission_control::mc_data::updates::artifacts::{Artifact, ArtifactKind, from_line};
use serde_json::{Value, json};

fn scan(line: Value) -> Vec<(ArtifactKind, String)> {
    let at = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
    from_line(&line, at)
        .into_iter()
        .map(|a: Artifact| {
            assert_eq!(a.at, at);
            (a.kind, a.reference)
        })
        .collect()
}

fn text(t: &str) -> Value {
    json!({"type":"assistant","message":{"content":[{"type":"text","text":t}]}})
}

fn tool_result(c: Value) -> Value {
    json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":c}]}})
}

fn tool_use(name: &str, input: Value) -> Value {
    json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":name,"input":input}]}})
}

#[test]
fn pr_url() {
    let got = scan(text(
        "see https://github.com/reflectionai/olympus/pull/41252.",
    ));
    assert_eq!(
        got,
        vec![(ArtifactKind::Pr, "reflectionai/olympus#41252".into())]
    );
}

#[test]
fn tickets() {
    let got = scan(text("fix MID-5521 and BLN-12, not XMID-1"));
    assert_eq!(
        got,
        vec![
            (ArtifactKind::Ticket, "MID-5521".into()),
            (ArtifactKind::Ticket, "BLN-12".into())
        ]
    );
}

#[test]
fn notion_and_slack() {
    let n = "https://app.notion.com/p/3e90075d4b99813c99b7c664923653de";
    let s = "https://reflectionai.slack.com/archives/C123/p1700000000000100";
    let got = scan(text(&format!("notes ({n}) and {s}")));
    assert_eq!(
        got,
        vec![
            (ArtifactKind::Notion, n.into()),
            (ArtifactKind::Slack, s.into())
        ]
    );
}

#[test]
fn commit_from_tool_result() {
    let got = scan(tool_result(json!("[master 1a2b3c4] msg\n 1 file changed")));
    assert_eq!(got, vec![(ArtifactKind::Commit, "1a2b3c4".into())]);
    let blocks = tool_result(json!([{"type":"text","text":"[main (root-commit) deadbeef0] init"}]));
    assert_eq!(
        scan(blocks),
        vec![(ArtifactKind::Commit, "deadbeef0".into())]
    );
    let jj = tool_result(json!("Parent commit (@-)      : qpvuntsm 1a2b3c4d5e6f msg"));
    assert_eq!(
        scan(jj),
        vec![(ArtifactKind::Commit, "1a2b3c4d5e6f".into())]
    );
    assert!(scan(text("fixed in 1a2b3c4 earlier")).is_empty());
}

#[test]
fn file_from_write_tool() {
    let w = tool_use(
        "Write",
        json!({"file_path":"/a/b.rs","content":"see https://github.com/x/y/pull/1 MID-9"}),
    );
    assert_eq!(scan(w), vec![(ArtifactKind::File, "/a/b.rs".into())]);
    let e = tool_use("Edit", json!({"file_path":"/a/c.rs","new_string":"MID-9"}));
    assert_eq!(scan(e), vec![(ArtifactKind::File, "/a/c.rs".into())]);
    let n = tool_use("NotebookEdit", json!({"notebook_path":"/a/n.ipynb"}));
    assert_eq!(scan(n), vec![(ArtifactKind::File, "/a/n.ipynb".into())]);
    let b = tool_use(
        "Bash",
        json!({"command":"gh pr view https://github.com/x/y/pull/7"}),
    );
    assert_eq!(scan(b), vec![(ArtifactKind::Pr, "x/y#7".into())]);
}

#[test]
fn dedup_within_line() {
    let got = scan(text(
        "MID-1 MID-1 https://github.com/x/y/pull/2 https://github.com/x/y/pull/2",
    ));
    assert_eq!(got.len(), 2);
}

#[test]
fn serde_shape() {
    let a = Artifact {
        kind: ArtifactKind::Pr,
        reference: "x/y#1".into(),
        at: Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
    };
    let v = serde_json::to_value(&a).unwrap();
    assert_eq!(v["kind"], "pr");
    assert_eq!(v["ref"], "x/y#1");
}
