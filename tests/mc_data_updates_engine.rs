use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use mission_control::mc_data::updates::engine::{EngineOpts, apply};
use mission_control::mc_data::updates::judge::Judge;
use mission_control::mc_data::updates::record::*;
use mission_control::mc_data::updates::tail::{AgentKind, ReadOutcome, Turn};
use serde_json::{Value, json};

const ROUTE: &str = "Which open task";
const STATE: &str = "Classify the task's state";
const REFINE: &str = "You maintain a short record";

/// Answers by prompt kind; logs every prompt.
struct FakeJudge {
    route: Mutex<String>,
    state: Mutex<String>,
    refine: Mutex<String>,
    fail: Mutex<bool>,
    log: Mutex<Vec<String>>,
}

impl FakeJudge {
    fn new() -> Self {
        Self {
            route: Mutex::new("NEW".into()),
            state: Mutex::new("working".into()),
            refine: Mutex::new(r#"{"intent":"refined goal","latest":"refined latest"}"#.into()),
            fail: Mutex::new(false),
            log: Mutex::new(vec![]),
        }
    }
    fn set(&self, slot: &Mutex<String>, v: &str) {
        *slot.lock().unwrap() = v.into();
    }
    fn calls(&self, marker: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains(marker))
            .count()
    }
}

#[async_trait]
impl Judge for FakeJudge {
    async fn ask(&self, prompt: &str, _max_tokens: u32) -> Result<String> {
        self.log.lock().unwrap().push(prompt.into());
        if *self.fail.lock().unwrap() {
            bail!("beam down");
        }
        let slot = if prompt.contains(ROUTE) {
            &self.route
        } else if prompt.contains(STATE) {
            &self.state
        } else if prompt.contains(REFINE) {
            &self.refine
        } else {
            bail!("unknown prompt")
        };
        Ok(slot.lock().unwrap().clone())
    }
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn min(m: i64) -> DateTime<Utc> {
    t0() + Duration::minutes(m)
}

fn file() -> SessionFile {
    SessionFile {
        host: "h".into(),
        session: "sess".into(),
        agent: "claude".into(),
        transcript: "/t.jsonl".into(),
        workspace: None,
        surface: None,
        status: None,
        segments: vec![],
        updated_at: t0(),
    }
}

fn user(at: DateTime<Utc>, text: &str) -> Turn {
    Turn::User {
        at,
        text: text.into(),
    }
}

fn agent(at: DateTime<Utc>, text: &str, kind: AgentKind) -> Turn {
    Turn::Agent {
        at,
        text: text.into(),
        kind,
    }
}

fn outcome(turns: Vec<Turn>, lines: Vec<Value>) -> ReadOutcome {
    ReadOutcome {
        turns,
        lines,
        next_offset: 0,
        reset: false,
    }
}

/// An assistant line mentioning PR `n` at `at`, with its reply turn.
fn pr_line(at: DateTime<Utc>, n: u32) -> (Turn, Value) {
    let text = format!("opened https://github.com/o/r/pull/{n}");
    let line = json!({"type":"assistant","timestamp":at.to_rfc3339(),
        "message":{"content":[{"type":"text","text":text}]}});
    (agent(at, &text, AgentKind::Reply), line)
}

fn opts(now: DateTime<Utc>) -> EngineOpts {
    EngineOpts { seed: false, now }
}

#[tokio::test]
async fn unrelated_messages_open_two_segments() {
    let j = FakeJudge::new();
    let mut f = file();
    let out = outcome(
        vec![
            user(
                min(0),
                "fix the login bug on the settings page please now ok",
            ),
            user(min(1), "separately write the quarterly report"),
        ],
        vec![],
    );
    let w = apply(&j, &mut f, &out, None, opts(min(1))).await;
    assert!(w.is_empty(), "{w:?}");
    let ids: Vec<_> = f.segments.iter().map(|s| s.segment.as_str()).collect();
    assert_eq!(ids, ["s1", "s2"]);
    // First message routes with no call; the second is routed against s1.
    assert_eq!(j.calls(ROUTE), 1);
    assert_eq!(f.segments[1].user_inputs.len(), 1);
    assert_eq!(f.segments[0].intent, "refined goal");
    assert_eq!(f.segments[0].latest_at, Some(min(0)));
}

#[tokio::test]
async fn new_segment_intent_is_first_ten_words_until_refined() {
    let j = FakeJudge::new();
    let mut f = file();
    let out = outcome(
        vec![user(
            min(0),
            "one two three four five six seven eight nine ten eleven",
        )],
        vec![],
    );
    apply(
        &j,
        &mut f,
        &out,
        None,
        EngineOpts {
            seed: true,
            now: min(0),
        },
    )
    .await;
    assert_eq!(
        f.segments[0].intent,
        "one two three four five six seven eight nine ten"
    );
}

#[tokio::test]
async fn related_message_joins_segment() {
    let j = FakeJudge::new();
    let mut f = file();
    j.set(&j.route, "s1");
    let out = outcome(
        vec![
            user(min(0), "design the rate limiter"),
            agent(min(1), "token bucket or leaky?", AgentKind::Reply),
            user(min(2), "token bucket"),
        ],
        vec![],
    );
    apply(&j, &mut f, &out, None, opts(min(2))).await;
    assert_eq!(f.segments.len(), 1);
    let s = &f.segments[0];
    assert_eq!(s.user_inputs.len(), 2);
    assert_eq!(s.agent_turns.len(), 1);
    assert_eq!(s.latest_at, Some(min(2)));
    // One refine per segment per apply, over the rendered turns.
    assert_eq!(j.calls(REFINE), 1);
    let log = j.log.lock().unwrap();
    let p = log.iter().find(|p| p.contains(REFINE)).unwrap();
    assert!(p.contains(
        "USER: design the rate limiter\nAGENT: token bucket or leaky?\nUSER: token bucket"
    ));
}

#[tokio::test]
async fn injected_turns_never_open_segments() {
    let j = FakeJudge::new();
    let mut f = file();
    let out = outcome(
        vec![agent(
            min(0),
            "Protocol: blin-letter/1 hi",
            AgentKind::Injected,
        )],
        vec![],
    );
    apply(&j, &mut f, &out, None, opts(min(0))).await;
    assert!(f.segments.is_empty());
    assert!(j.log.lock().unwrap().is_empty());

    // Held turns attach to the segment the next user turn creates.
    let out = outcome(
        vec![
            agent(min(1), "peer note", AgentKind::Injected),
            user(min(2), "do the thing"),
            agent(min(3), "task-notification", AgentKind::Injected),
        ],
        vec![],
    );
    apply(&j, &mut f, &out, None, opts(min(3))).await;
    assert_eq!(f.segments.len(), 1);
    let s = &f.segments[0];
    assert_eq!(s.user_inputs.len(), 1);
    assert_eq!(s.agent_turns.len(), 2);
    assert!(s.agent_turns.iter().all(|t| t.injected));
    let log = j.log.lock().unwrap();
    assert!(log.iter().any(|p| p.contains("AGENT(loop): peer note")));
}

#[tokio::test]
async fn agent_turn_while_user_active_does_not_refine() {
    let j = FakeJudge::new();
    let mut f = file();
    apply(
        &j,
        &mut f,
        &outcome(vec![user(min(0), "build it")], vec![]),
        None,
        opts(min(0)),
    )
    .await;
    assert_eq!(j.calls(REFINE), 1);
    j.set(&j.state, "waiting_on_me");
    let (turn, line) = pr_line(min(30), 7);
    apply(
        &j,
        &mut f,
        &outcome(vec![turn], vec![line]),
        None,
        opts(min(30)),
    )
    .await;
    assert_eq!(j.calls(STATE), 2);
    assert_eq!(j.calls(REFINE), 1, "within 2h of the user's message");
    let s = &f.segments[0];
    assert_eq!(s.state, SegState::WaitingOnMe);
    assert_eq!(s.artifacts.len(), 1);
    assert_eq!(s.latest_at, Some(min(30)));
}

#[tokio::test]
async fn after_two_hours_artifact_triggers_one_refine_per_hour() {
    let j = FakeJudge::new();
    let mut f = file();
    apply(
        &j,
        &mut f,
        &outcome(vec![user(min(0), "ship it")], vec![]),
        None,
        opts(min(0)),
    )
    .await;
    assert_eq!(j.calls(REFINE), 1);
    for (m, n, want) in [(125, 1, 2), (150, 2, 2), (190, 3, 3)] {
        let (turn, line) = pr_line(min(m), n);
        apply(
            &j,
            &mut f,
            &outcome(vec![turn], vec![line]),
            None,
            opts(min(m)),
        )
        .await;
        assert_eq!(j.calls(REFINE), want, "at +{m}m");
    }
    assert_eq!(f.segments[0].last_refine_at, Some(min(190)));
    assert_eq!(f.segments[0].artifacts.len(), 3);

    // A repeated artifact is not new: no refine even an hour later.
    let (turn, line) = pr_line(min(260), 3);
    apply(
        &j,
        &mut f,
        &outcome(vec![turn], vec![line]),
        None,
        opts(min(260)),
    )
    .await;
    assert_eq!(j.calls(REFINE), 3);
}

#[tokio::test]
async fn after_two_hours_state_change_triggers_refine() {
    let j = FakeJudge::new();
    let mut f = file();
    apply(
        &j,
        &mut f,
        &outcome(vec![user(min(0), "ship it")], vec![]),
        None,
        opts(min(0)),
    )
    .await;
    let reply = |m| outcome(vec![agent(min(m), "progress", AgentKind::Reply)], vec![]);
    apply(&j, &mut f, &reply(130), None, opts(min(130))).await;
    assert_eq!(j.calls(REFINE), 1, "working → working is no change");
    j.set(&j.state, "done");
    apply(&j, &mut f, &reply(140), None, opts(min(140))).await;
    assert_eq!(j.calls(REFINE), 2);
    assert_eq!(f.segments[0].state, SegState::Done);
}

#[tokio::test]
async fn busy_skips_state_call() {
    let j = FakeJudge::new();
    let mut f = file();
    j.set(&j.state, "blocked");
    let out = outcome(
        vec![user(min(0), "go"), agent(min(1), "on it", AgentKind::Reply)],
        vec![],
    );
    apply(&j, &mut f, &out, Some(true), opts(min(1))).await;
    assert_eq!(j.calls(STATE), 0);
    assert_eq!(f.segments[0].state, SegState::Working);
    j.set(&j.route, "s1");
    let out = outcome(vec![agent(min(2), "error", AgentKind::Reply)], vec![]);
    apply(&j, &mut f, &out, Some(false), opts(min(2))).await;
    assert_eq!(j.calls(STATE), 1);
    assert_eq!(f.segments[0].state, SegState::Blocked);
}

#[tokio::test]
async fn seed_mode_never_refines() {
    let j = FakeJudge::new();
    let mut f = file();
    let (turn, line) = pr_line(min(200), 1);
    let out = outcome(
        vec![user(min(0), "ship it"), turn, user(min(300), "and more")],
        vec![line],
    );
    j.set(&j.route, "s1");
    apply(
        &j,
        &mut f,
        &out,
        None,
        EngineOpts {
            seed: true,
            now: min(300),
        },
    )
    .await;
    assert_eq!(j.calls(REFINE), 0);
    assert_eq!(f.segments[0].intent, "ship it");
    assert_eq!(f.segments[0].last_refine_at, None);
}

#[tokio::test]
async fn judge_error_keeps_previous_intent() {
    let j = FakeJudge::new();
    let mut f = file();
    apply(
        &j,
        &mut f,
        &outcome(vec![user(min(0), "ship it")], vec![]),
        None,
        opts(min(0)),
    )
    .await;
    let before = f.segments[0].clone();
    *j.fail.lock().unwrap() = true;
    let w = apply(
        &j,
        &mut f,
        &outcome(vec![user(min(5), "also this")], vec![]),
        None,
        opts(min(5)),
    )
    .await;
    assert!(w.iter().any(|w| w.starts_with("route failed")), "{w:?}");
    assert!(w.iter().any(|w| w.starts_with("state failed")), "{w:?}");
    assert!(w.iter().any(|w| w.starts_with("refine failed")), "{w:?}");
    // The message still lands (in the current segment); old values stay.
    assert_eq!(f.segments.len(), 1);
    let s = &f.segments[0];
    assert_eq!(s.user_inputs.len(), 2);
    assert_eq!((&s.intent, &s.latest), (&before.intent, &before.latest));
    assert_eq!(s.state, before.state);
    assert_eq!(s.last_refine_at, before.last_refine_at);
}

#[tokio::test]
async fn segment_older_than_48h_not_routed() {
    let j = FakeJudge::new();
    let mut f = file();
    j.set(&j.route, "s1");
    apply(
        &j,
        &mut f,
        &outcome(vec![user(min(0), "old work")], vec![]),
        None,
        opts(min(0)),
    )
    .await;
    let later = min(49 * 60);
    apply(
        &j,
        &mut f,
        &outcome(vec![user(later, "fresh work")], vec![]),
        None,
        opts(later),
    )
    .await;
    assert_eq!(j.calls(ROUTE), 0, "no open segment, so no route call");
    assert_eq!(f.segments.len(), 2);

    // Done segments are not routed either.
    f.segments[1].state = SegState::Done;
    let m = later + Duration::minutes(1);
    apply(
        &j,
        &mut f,
        &outcome(vec![user(m, "third")], vec![]),
        None,
        opts(m),
    )
    .await;
    assert_eq!(j.calls(ROUTE), 0);
    assert_eq!(f.segments.len(), 3);
    assert_eq!(f.segments[2].segment, "s3");
}
