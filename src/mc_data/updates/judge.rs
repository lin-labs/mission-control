//! Beam prompts for the transcript watcher: route a message to a segment,
//! classify a segment's state, refine its intent/latest.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Semaphore;

use super::record::{SegState, Segment};
use crate::llm::short_text::ShortTextClient;

#[async_trait]
pub trait Judge: Send + Sync {
    async fn ask(&self, prompt: &str, max_tokens: u32) -> Result<String>;
}

/// Process-wide bound on Beam calls in flight.
static BEAM_CALLS: Semaphore = Semaphore::const_new(4);

#[async_trait]
impl Judge for ShortTextClient {
    async fn ask(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        let _permit = BEAM_CALLS.acquire().await?;
        self.complete(prompt, max_tokens).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Existing(String),
    New,
}

const ROUTE_PROMPT: &str = "\
You group one user's messages in an agent chat into TASKS, so the user can later see what they were trying to get done.

A TASK is one deliverable or goal (e.g. \"design feature X\", \"fix bug Y\", \"write doc Z\").
Everything below stays in the SAME task as the goal it serves:
- answers to the agent's questions, corrections, \"actually...\", added constraints or preferences
- research or side questions asked to make a decision for that goal (which model, how many GPUs, what an endpoint returns)
- requests to refine, test, or extend the agent's output for that goal
Start a NEW task only when the user turns to a different deliverable that would still make sense if the earlier goal did not exist.
If unsure, keep it in the current task.

Open tasks:
{open}

New user message:
{message}

Which open task does the new message continue? Reply with exactly one segment id (e.g. S2) or NEW.";

const STATE_PROMPT: &str = "\
Below are the most recent turns of an agent chat about one task. Classify the task's state right now:
working: the agent is still working or the next step is the agent's
waiting_on_me: the agent asked the user a question or needs the user's decision or input
blocked: the agent cannot proceed because of an error or missing access
done: the deliverable is finished

Reply with exactly one of: working|waiting_on_me|blocked|done

Recent turns:
{recent}";

const REFINE_PROMPT: &str = "\
You maintain a short record of one task in an agent chat.

Current intent: {intent}
Current latest: {latest}

New turns:
{turns}

Keep the current intent wording unless the user's goal changed. The intent is at most 10 words, in the user's own words. The latest is at most 15 words, the newest progress.
Return only JSON: {\"intent\":\"...\",\"latest\":\"...\"}";

const ROUTE_TOKENS: u32 = 4_000;
const STATE_TOKENS: u32 = 4_000;
const REFINE_TOKENS: u32 = 6_000;

pub async fn route(j: &dyn Judge, message: &str, open: &[(String, String)]) -> Result<Route> {
    if open.is_empty() {
        return Ok(Route::New);
    }
    let list = open
        .iter()
        .map(|(id, intent)| format!("{id}: {intent}"))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = ROUTE_PROMPT
        .replace("{open}", &list)
        .replace("{message}", message);
    let raw = j.ask(&prompt, ROUTE_TOKENS).await?;
    Ok(parse_route(&raw, open))
}

/// First token that is an open id or NEW; anything else is NEW.
fn parse_route(raw: &str, open: &[(String, String)]) -> Route {
    for tok in raw.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.eq_ignore_ascii_case("new") {
            return Route::New;
        }
        if let Some((id, _)) = open.iter().find(|(id, _)| id.eq_ignore_ascii_case(tok)) {
            return Route::Existing(id.clone());
        }
    }
    Route::New
}

pub async fn classify_state(j: &dyn Judge, recent: &str) -> Result<SegState> {
    let raw = j
        .ask(&STATE_PROMPT.replace("{recent}", recent), STATE_TOKENS)
        .await?;
    parse_state(&raw)
}

fn parse_state(raw: &str) -> Result<SegState> {
    let lower = raw
        .to_ascii_lowercase()
        .replace("waiting on me", "waiting_on_me");
    [
        ("working", SegState::Working),
        ("waiting_on_me", SegState::WaitingOnMe),
        ("blocked", SegState::Blocked),
        ("done", SegState::Done),
    ]
    .into_iter()
    .filter_map(|(name, st)| lower.find(name).map(|at| (at, st)))
    .min_by_key(|(at, _)| *at)
    .map(|(_, st)| st)
    .with_context(|| {
        format!(
            "state: no value in {:?}",
            raw.chars().take(80).collect::<String>()
        )
    })
}

pub async fn refine(j: &dyn Judge, seg: &Segment, new_turns: &str) -> Result<(String, String)> {
    let prompt = REFINE_PROMPT
        .replace("{intent}", &seg.intent)
        .replace("{latest}", &seg.latest)
        .replace("{turns}", new_turns);
    let raw = j.ask(&prompt, REFINE_TOKENS).await?;
    parse_refine(&raw)
}

fn parse_refine(raw: &str) -> Result<(String, String)> {
    let s = raw.trim();
    let json = match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => &s[a..=b],
        _ => bail!(
            "refine: no JSON object in {:?}",
            s.chars().take(80).collect::<String>()
        ),
    };
    #[derive(Deserialize)]
    struct Raw {
        intent: String,
        latest: String,
    }
    let p: Raw = serde_json::from_str(json).context("refine parse")?;
    Ok((truncate_words(&p.intent, 10), truncate_words(&p.latest, 15)))
}

fn truncate_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeJudge {
        reply: Mutex<String>,
        calls: AtomicUsize,
    }

    impl FakeJudge {
        fn new(reply: &str) -> Self {
            Self {
                reply: Mutex::new(reply.into()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Judge for FakeJudge {
        async fn ask(&self, _prompt: &str, _max_tokens: u32) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.reply.lock().unwrap().clone())
        }
    }

    fn open() -> Vec<(String, String)> {
        vec![
            ("S1".into(), "fix login bug".into()),
            ("S2".into(), "write design doc".into()),
        ]
    }

    fn seg() -> Segment {
        Segment {
            segment: "S1".into(),
            intent: "fix login bug".into(),
            latest: "started".into(),
            latest_at: None,
            state: SegState::Working,
            needs_me: None,
            user_inputs: vec![],
            agent_turns: vec![],
            artifacts: vec![],
            last_refine_at: None,
        }
    }

    #[tokio::test]
    async fn route_parses_existing() {
        let j = FakeJudge::new("  \"s2\"\n");
        assert_eq!(
            route(&j, "m", &open()).await.unwrap(),
            Route::Existing("S2".into())
        );
        let j = FakeJudge::new("The message continues S1.");
        assert_eq!(
            route(&j, "m", &open()).await.unwrap(),
            Route::Existing("S1".into())
        );
    }

    #[tokio::test]
    async fn route_parses_new() {
        let j = FakeJudge::new("NEW");
        assert_eq!(route(&j, "m", &open()).await.unwrap(), Route::New);
    }

    #[tokio::test]
    async fn route_unknown_id_is_new() {
        for reply in ["S9", "no idea"] {
            let j = FakeJudge::new(reply);
            assert_eq!(route(&j, "m", &open()).await.unwrap(), Route::New);
        }
    }

    #[tokio::test]
    async fn route_no_open_segments_skips_call() {
        let j = FakeJudge::new("S1");
        assert_eq!(route(&j, "m", &[]).await.unwrap(), Route::New);
        assert_eq!(j.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn state_parses_each_value() {
        for (reply, want) in [
            ("working", SegState::Working),
            (" \"waiting_on_me\" ", SegState::WaitingOnMe),
            ("Waiting on me.", SegState::WaitingOnMe),
            ("Blocked", SegState::Blocked),
            ("The state is done", SegState::Done),
        ] {
            let j = FakeJudge::new(reply);
            assert_eq!(classify_state(&j, "x").await.unwrap(), want, "{reply}");
        }
    }

    #[tokio::test]
    async fn state_garbage_is_error() {
        let j = FakeJudge::new("hmm");
        assert!(classify_state(&j, "x").await.is_err());
    }

    #[tokio::test]
    async fn refine_parses_json_and_truncates() {
        let j = FakeJudge::new(
            "Sure:\n{\"intent\":\"one two three four five six seven eight nine ten eleven twelve\",\
             \"latest\":\"a b c d e f g h i j k l m n o p q\"}",
        );
        let (intent, latest) = refine(&j, &seg(), "turns").await.unwrap();
        assert_eq!(intent, "one two three four five six seven eight nine ten");
        assert_eq!(latest, "a b c d e f g h i j k l m n o");
        let j = FakeJudge::new("{\"intent\":\"x\"}");
        assert!(refine(&j, &seg(), "t").await.is_err());
    }

    #[tokio::test]
    #[ignore = "live Beam call; needs REFLECTION_API_KEY in ~/.zshenv"]
    async fn beam_routes_design_chat() {
        use crate::llm::short_text::{BEAM_DEFAULT_BASE_URL, BEAM_DEFAULT_MODEL};
        let key = crate::llm::openai::resolve_reflection_api_key(None)
            .await
            .api_key
            .expect("REFLECTION_API_KEY");
        let c = ShortTextClient::beam(key, BEAM_DEFAULT_MODEL.into(), BEAM_DEFAULT_BASE_URL.into());
        let open = vec![
            (
                "S1".to_string(),
                "design a rate limiter for the API".to_string(),
            ),
            (
                "S2".to_string(),
                "fix flaky CSS build on the docs site".to_string(),
            ),
        ];
        let r = route(
            &c,
            "which algorithm should the rate limiter use, token bucket or leaky bucket?",
            &open,
        )
        .await
        .unwrap();
        println!("route: {r:?}");
        assert_eq!(r, Route::Existing("S1".into()));
    }
}
