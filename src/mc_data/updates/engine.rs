//! Applies newly read transcript turns to a session record: routes each user
//! message to a segment, attaches agent turns and artifacts, classifies state,
//! and refines intent/latest when the trigger rule fires.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use super::artifacts::{Artifact, from_line};
use super::judge::{Judge, Route, classify_state, refine, route};
use super::record::{AgentTurn, SegState, Segment, SessionFile, UserInput};
use super::tail::{AgentKind, ReadOutcome, Turn};

const OPEN_HOURS: i64 = 48;
const QUIET_HOURS: i64 = 2;
const REFINE_EVERY_HOURS: i64 = 1;
const STATE_TURNS: usize = 3;
const NEW_INTENT_WORDS: usize = 10;

pub struct EngineOpts {
    pub seed: bool,
    pub now: DateTime<Utc>,
}

/// What this apply added to one segment.
#[derive(Default)]
struct Touched {
    /// Rendered turns, oldest first.
    turns: Vec<String>,
    /// Rendered turns and new artifacts, for refine.
    lines: Vec<String>,
    user_msg: bool,
    new_artifact: bool,
}

/// Agent turns and artifacts seen before the session's first user input.
#[derive(Default)]
struct Pending {
    turns: Vec<(AgentTurn, String)>,
    artifacts: Vec<Artifact>,
}

pub async fn apply(
    j: &dyn Judge,
    file: &mut SessionFile,
    out: &ReadOutcome,
    agent_busy: Option<bool>,
    opts: EngineOpts,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut touched: BTreeMap<usize, Touched> = BTreeMap::new();
    let mut pending = Pending::default();
    let segs = &mut file.segments;
    let mut cur = (0..segs.len()).max_by_key(|&i| segs[i].user_inputs.last().map(|u| u.at));

    // Merge lines (for artifacts) and turns by time; a line at a turn's time
    // follows the turn, so a link in a user's message joins its segment.
    let first_at = out.turns.first().map(turn_at).unwrap_or(opts.now);
    let mut last_at = None;
    let mut lines = out.lines.iter().peekable();
    for turn in out.turns.iter().map(Some).chain([None]) {
        while let Some(&line) = lines.peek() {
            let at = line_at(line).or(last_at).unwrap_or(first_at);
            if turn.is_some_and(|t| at >= turn_at(t)) {
                break;
            }
            lines.next();
            last_at = Some(at);
            for a in from_line(line, at) {
                match cur {
                    Some(i) => add_artifact(&mut segs[i], touched.entry(i).or_default(), a),
                    None if !pending.artifacts.contains(&a) => pending.artifacts.push(a),
                    None => {}
                }
            }
        }
        let Some(turn) = turn else { break };
        last_at = Some(turn_at(turn));
        match turn {
            Turn::User { at, text } => {
                let open: Vec<_> = segs
                    .iter()
                    .filter(|s| {
                        s.state != SegState::Done
                            && activity(s).is_some_and(|a| *at - a < Duration::hours(OPEN_HOURS))
                    })
                    .map(|s| (s.segment.clone(), s.intent.clone()))
                    .collect();
                let target = match route(j, text, &open).await {
                    Ok(Route::Existing(id)) => segs.iter().position(|s| s.segment == id),
                    Ok(Route::New) => None,
                    Err(e) => {
                        warnings.push(format!("route failed: {e}"));
                        cur.filter(|&i| open.iter().any(|(id, _)| *id == segs[i].segment))
                    }
                };
                let i = target.unwrap_or_else(|| {
                    segs.push(new_segment(segs, text));
                    segs.len() - 1
                });
                let t = touched.entry(i).or_default();
                for (turn, line) in pending.turns.drain(..) {
                    segs[i].agent_turns.push(turn);
                    t.turns.push(line.clone());
                    t.lines.push(line);
                }
                for a in pending.artifacts.drain(..) {
                    add_artifact(&mut segs[i], t, a);
                }
                segs[i].user_inputs.push(UserInput {
                    at: *at,
                    text: text.clone(),
                });
                let line = format!("USER: {text}");
                t.turns.push(line.clone());
                t.lines.push(line);
                t.user_msg = true;
                cur = Some(i);
            }
            Turn::Agent { at, text, kind } => {
                let injected = *kind == AgentKind::Injected;
                let line = format!("AGENT{}: {text}", if injected { "(loop)" } else { "" });
                let turn = AgentTurn { at: *at, injected };
                match cur {
                    Some(i) => {
                        segs[i].agent_turns.push(turn);
                        let t = touched.entry(i).or_default();
                        t.turns.push(line.clone());
                        t.lines.push(line);
                    }
                    None => pending.turns.push((turn, line)),
                }
            }
        }
    }

    for (i, t) in touched {
        let seg = &mut segs[i];
        let newest_user = seg.user_inputs.iter().map(|u| u.at).max();
        let newest = newest_user.max(seg.agent_turns.iter().map(|a| a.at).max());
        if newest.is_some() {
            seg.latest_at = newest;
        }

        let old = seg.state;
        if agent_busy == Some(true) && cur == Some(i) {
            seg.state = SegState::Working;
        } else if !t.turns.is_empty() {
            let recent = t.turns[t.turns.len().saturating_sub(STATE_TURNS)..].join("\n");
            match classify_state(j, &recent).await {
                Ok(s) => seg.state = s,
                Err(e) => warnings.push(format!("state failed: {e}")),
            }
        }
        if opts.seed {
            continue;
        }
        let needs_attention = seg.state != old && seg.state != SegState::Working;
        let quiet = newest_user.is_none_or(|u| opts.now - u >= Duration::hours(QUIET_HOURS));
        let due = seg
            .last_refine_at
            .is_none_or(|r| opts.now - r >= Duration::hours(REFINE_EVERY_HOURS));
        if t.user_msg || (quiet && due && (needs_attention || t.new_artifact)) {
            match refine(j, seg, &t.lines.join("\n")).await {
                Ok((intent, latest)) => {
                    seg.intent = intent;
                    seg.latest = latest;
                    seg.last_refine_at = Some(opts.now);
                }
                Err(e) => warnings.push(format!("refine failed: {e}")),
            }
        }
    }
    warnings
}

fn turn_at(t: &Turn) -> DateTime<Utc> {
    match t {
        Turn::User { at, .. } | Turn::Agent { at, .. } => *at,
    }
}

/// Same parsing as `tail::read_new`.
fn line_at(line: &Value) -> Option<DateTime<Utc>> {
    let s = line.get("timestamp")?.as_str()?;
    Some(DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Utc))
}

fn activity(s: &Segment) -> Option<DateTime<Utc>> {
    let user = s.user_inputs.last().map(|u| u.at);
    s.latest_at
        .max(user)
        .max(s.agent_turns.last().map(|a| a.at))
}

/// Adds `a` unless the segment already has that reference.
fn add_artifact(seg: &mut Segment, t: &mut Touched, a: Artifact) {
    let seen = seg
        .artifacts
        .iter()
        .any(|x| x.kind == a.kind && x.reference == a.reference);
    if !seen {
        t.lines.push(format!("ARTIFACT: {}", a.reference));
        t.new_artifact = true;
        seg.artifacts.push(a);
    }
}

fn new_segment(segs: &[Segment], text: &str) -> Segment {
    let n = segs
        .iter()
        .filter_map(|s| s.segment.strip_prefix('s')?.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    Segment {
        segment: format!("s{}", n + 1),
        intent: text
            .split_whitespace()
            .take(NEW_INTENT_WORDS)
            .collect::<Vec<_>>()
            .join(" "),
        latest: String::new(),
        latest_at: None,
        state: SegState::Working,
        needs_me: None,
        user_inputs: vec![],
        agent_turns: vec![],
        artifacts: vec![],
        last_refine_at: None,
    }
}
