//! One-shot provider client for short, structured string generations.
//!
//! xAI uses its OpenAI-compatible Chat Completions endpoint. OpenAI uses the
//! Responses API. The selected provider is machine-local and authoritative;
//! callers retain their deterministic/no-result fallbacks when it fails.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::json;

const XAI_ENDPOINT: &str = "https://api.x.ai/v1/chat/completions";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/responses";
/// Default xAI model. Their lineup shifts; this name is the current
/// "small + fast" tier (verified live 2026-05-25 against
/// https://api.x.ai/v1/models).
const DEFAULT_MODEL: &str = "grok-4-fast-non-reasoning";

#[derive(Clone)]
pub struct ShortTextClient {
    http: reqwest::Client,
    transport: Transport,
}

#[derive(Clone)]
enum Transport {
    Xai { api_key: String },
    Openai { api_key: String, model: String },
}

#[derive(Serialize)]
struct XaiRequest<'a> {
    model: &'a str,
    messages: Vec<XaiMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct XaiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct XaiResponse {
    choices: Vec<XaiChoice>,
}

#[derive(Deserialize)]
struct XaiChoice {
    message: XaiChoiceMessage,
}

#[derive(Deserialize)]
struct XaiChoiceMessage {
    content: String,
}

#[derive(Deserialize)]
struct OpenaiResponse {
    #[serde(default)]
    output: Vec<OpenaiOutputItem>,
}

#[derive(Deserialize)]
struct OpenaiOutputItem {
    #[serde(default)]
    content: Vec<OpenaiContentItem>,
}

#[derive(Deserialize)]
struct OpenaiContentItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

impl ShortTextClient {
    pub fn xai(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            transport: Transport::Xai { api_key },
        }
    }

    pub fn openai(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            transport: Transport::Openai { api_key, model },
        }
    }

    pub fn provider_name(&self) -> &'static str {
        match self.transport {
            Transport::Xai { .. } => "xAI",
            Transport::Openai { .. } => "OpenAI",
        }
    }

    async fn complete(&self, user_prompt: &str, max_tokens: u32) -> Result<String> {
        match &self.transport {
            Transport::Xai { api_key } => {
                let body = XaiRequest {
                    model: DEFAULT_MODEL,
                    messages: vec![XaiMessage {
                        role: "user",
                        content: user_prompt,
                    }],
                    temperature: 0.0,
                    max_tokens,
                };
                let resp = self
                    .http
                    .post(XAI_ENDPOINT)
                    .bearer_auth(api_key)
                    .json(&body)
                    .send()
                    .await
                    .context("xAI request failed")?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("xAI HTTP {}: {}", status, text);
                }
                let parsed: XaiResponse = resp.json().await.context("xAI parse")?;
                parsed
                    .choices
                    .into_iter()
                    .next()
                    .map(|choice| choice.message.content)
                    .ok_or_else(|| anyhow!("xAI returned no choices"))
            }
            Transport::Openai { api_key, model } => {
                let body = json!({
                    "model": model,
                    "input": user_prompt,
                    "instructions": "Follow the requested output format exactly.",
                    "reasoning": { "effort": "none" },
                    // Responses counts reasoning and visible output together;
                    // 16 is the practical floor for even a three-letter reply.
                    "max_output_tokens": max_tokens.max(16),
                });
                let resp = self
                    .http
                    .post(OPENAI_ENDPOINT)
                    .bearer_auth(api_key)
                    .json(&body)
                    .send()
                    .await
                    .context("OpenAI request failed")?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("OpenAI HTTP {}: {}", status, text);
                }
                let parsed: OpenaiResponse = resp.json().await.context("OpenAI parse")?;
                extract_openai_output_text(parsed)
                    .ok_or_else(|| anyhow!("OpenAI returned no output_text"))
            }
        }
    }
}

fn extract_openai_output_text(response: OpenaiResponse) -> Option<String> {
    response
        .output
        .into_iter()
        .flat_map(|item| item.content)
        .find(|item| item.kind == "output_text" && !item.text.trim().is_empty())
        .map(|item| item.text)
}

/// Generate a 3-letter uppercase prefix for a workspace name. Avoids any
/// prefix in `used_prefixes` (the caller passes the set of prefixes already
/// assigned to other workspaces this session).
///
/// On success returns the validated 3-letter code. On any failure (network,
/// invalid output, second-try collision) returns an error; the caller is
/// expected to fall through to the deterministic helper.
pub async fn generate_workspace_prefix(
    client: &ShortTextClient,
    workspace_name: &str,
    used_prefixes: &[String],
) -> Result<String> {
    let used_list = if used_prefixes.is_empty() {
        "(none)".to_string()
    } else {
        used_prefixes.join(", ")
    };

    let attempt = |extra_avoid: Option<&str>| -> String {
        let avoid_clause = match extra_avoid {
            Some(bad) => format!(
                " Also avoid '{}' (you suggested that and it was rejected).",
                bad
            ),
            None => String::new(),
        };
        format!(
            "Generate a 3-character uppercase alphabetic abbreviation for the workspace named \"{}\". \
             The code must be exactly 3 ASCII uppercase letters (A-Z) and must NOT be one of: {}.{}  \
             Output ONLY the 3-letter code, no quotes, no punctuation, no explanation.",
            workspace_name, used_list, avoid_clause
        )
    };

    // First try.
    let raw = client.complete(&attempt(None), 8).await?;
    if let Some(p) = validate(&raw, used_prefixes) {
        return Ok(p);
    }

    // Second try, telling the model what it just got wrong.
    let raw2 = client.complete(&attempt(Some(&raw)), 8).await?;
    if let Some(p) = validate(&raw2, used_prefixes) {
        return Ok(p);
    }

    Err(anyhow!(
        "{} returned invalid prefixes twice (last raw output: {:?})",
        client.provider_name(),
        raw2.chars().take(40).collect::<String>()
    ))
}

/// Infer a surface's intent (`overall_goal` + `latest_ask`) from a merged
/// terminal transcript. Used for remote (mosh/ssh) surfaces, whose content can
/// only be observed via the screen — see `frame_merge` / `remote_intent`.
/// Handles BOTH an AI agent (Claude/Codex) and a plain remote shell: for a
/// shell, `latest_ask` is the most recent command the user ran.
///
/// The prompt is hardened against the classic screen-grab mistake: command
/// OUTPUT, input-box placeholders, suggestions, and unsent typing must NOT be
/// treated as the user action. Returns empty fields rather than guessing when
/// no genuine user input/command is present.
pub async fn infer_intent(
    client: &ShortTextClient,
    transcript: &str,
) -> Result<crate::mc_data::surface_render::SurfaceIntentSummary> {
    // Cap input: the tail holds the most recent (and most relevant) turns.
    let tail = {
        let lines: Vec<&str> = transcript.lines().collect();
        let start = lines.len().saturating_sub(200);
        lines[start..].join("\n")
    };
    let prompt = format!(
        "You are reading a transcript captured from a terminal pane. It may be an AI coding agent \
         (Claude/Codex — a human user and an AI assistant) OR a plain shell session (local or over \
         ssh/mosh). Extract two things:\n\
         - overall_goal: what the user is doing in this session overall — the task or goal (<=90 chars).\n\
         - latest_ask: the MOST RECENT meaningful USER action (<=90 chars) — i.e. the latest prompt \
         the user submitted to an agent, OR, for a shell, the latest command the user actually RAN \
         at a shell prompt.\n\
         Count only genuine user input. Do NOT treat command OUTPUT, the agent's own messages, \
         input-box placeholder/autocomplete/suggestion text, in-progress (unsent) typing, or menus \
         as the user action. If you cannot find any genuine user input or command, use null.\n\
         Output ONLY compact JSON (no prose, no code fences): \
         {{\"overall_goal\": <string|null>, \"latest_ask\": <string|null>}}.\n\n\
         Transcript:\n{tail}"
    );
    let raw = client.complete(&prompt, 200).await?;
    parse_intent(&raw)
}

fn parse_intent(raw: &str) -> Result<crate::mc_data::surface_render::SurfaceIntentSummary> {
    use crate::mc_data::surface_render::SurfaceIntentSummary;
    let s = raw.trim();
    // Be lenient: pull the first {...} object even if the model wrapped it.
    let json = match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => &s[a..=b],
        _ => anyhow::bail!(
            "short-text intent: no JSON object in {:?}",
            s.chars().take(80).collect::<String>()
        ),
    };
    #[derive(Deserialize)]
    struct RawIntent {
        #[serde(default)]
        overall_goal: Option<String>,
        #[serde(default)]
        latest_ask: Option<String>,
    }
    let parsed: RawIntent = serde_json::from_str(json).context("short-text intent parse")?;
    let clean = |o: Option<String>| {
        o.map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
    };
    Ok(SurfaceIntentSummary {
        overall_goal: clean(parsed.overall_goal),
        latest_ask: clean(parsed.latest_ask),
    })
}

/// Summarize what the user is trying to accomplish across a session into one
/// short line, for a surface's "overall" goal. Input is the session's user
/// turns (most recent last). Returns a trimmed one-liner (<=90 chars-ish).
pub async fn summarize_overall(client: &ShortTextClient, user_turns: &[String]) -> Result<String> {
    if user_turns.is_empty() {
        anyhow::bail!("no user turns to summarize");
    }
    // Cap input: keep the latest turns (they reflect the current direction)
    // plus the first (the original goal).
    let mut joined = String::new();
    if let Some(first) = user_turns.first() {
        joined.push_str("[first] ");
        joined.push_str(&first.chars().take(400).collect::<String>());
        joined.push('\n');
    }
    for t in user_turns.iter().rev().take(8).rev() {
        joined.push_str("- ");
        joined.push_str(&t.chars().take(400).collect::<String>());
        joined.push('\n');
    }
    let prompt = format!(
        "These are the user's messages in a coding-agent session (oldest first, \
         newest last). In ONE short line (<=90 chars), state what the user is \
         ultimately trying to accomplish overall — the session's goal, accounting \
         for how it has evolved. Output ONLY the line, no quotes, no prefix.\n\n{joined}"
    );
    let raw = client.complete(&prompt, 64).await?;
    let line = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '`')
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if line.is_empty() {
        anyhow::bail!("{} returned empty overall summary", client.provider_name());
    }
    Ok(line)
}

/// Returns the canonical prefix if `raw` cleans to a 3-letter uppercase code
/// not in `used`. Otherwise None.
pub fn validate(raw: &str, used: &[String]) -> Option<String> {
    // Strip whitespace, quotes, backticks, periods.
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if cleaned.len() != 3 {
        return None;
    }
    if used.iter().any(|u| u.eq_ignore_ascii_case(&cleaned)) {
        return None;
    }
    Some(cleaned)
}

/// Deterministic fallback prefix derivation. Always returns *some* 3-letter
/// code (or a digit-suffixed code on collision). Used when the selected
/// provider is unavailable or returns garbage twice.
pub fn deterministic_prefix(workspace_name: &str, used_prefixes: &[String]) -> String {
    // 1. Strip non-alphabetic, uppercase. Then prefer the first letter plus
    //    the next 2 consonants. Pad with vowels (then 'X') if too short.
    let upper: String = workspace_name
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();

    fn is_consonant(c: char) -> bool {
        c.is_ascii_alphabetic() && !matches!(c, 'A' | 'E' | 'I' | 'O' | 'U')
    }

    let mut picked = String::new();
    // Always take the first character if present.
    if let Some(first) = upper.chars().next() {
        picked.push(first);
    }
    // Then up to 2 consonants from the remaining characters.
    for c in upper.chars().skip(1) {
        if picked.len() >= 3 {
            break;
        }
        if is_consonant(c) {
            picked.push(c);
        }
    }
    // Pad with vowels from the remaining characters.
    if picked.len() < 3 {
        for c in upper.chars().skip(1) {
            if picked.len() >= 3 {
                break;
            }
            if !is_consonant(c) && !picked.contains(c) {
                picked.push(c);
            }
        }
    }
    while picked.len() < 3 {
        picked.push('X');
    }
    let candidate = picked.chars().take(3).collect::<String>();

    // Dedup against used prefixes by appending a digit.
    if !used_prefixes
        .iter()
        .any(|u| u.eq_ignore_ascii_case(&candidate))
    {
        return candidate;
    }
    for n in 2u32..=99 {
        let extended = format!("{}{}", candidate, n);
        if !used_prefixes
            .iter()
            .any(|u| u.eq_ignore_ascii_case(&extended))
        {
            return extended;
        }
    }
    candidate // give up; collision is the caller's problem
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_names_match_selected_transport() {
        assert_eq!(ShortTextClient::xai("x".into()).provider_name(), "xAI");
        assert_eq!(
            ShortTextClient::openai("x".into(), "gpt-5.4-mini".into()).provider_name(),
            "OpenAI"
        );
    }

    #[test]
    fn openai_response_extracts_output_text_content() {
        let response: OpenaiResponse = serde_json::from_value(json!({
            "output": [{
                "type": "message",
                "content": [
                    {"type": "refusal", "text": ""},
                    {"type": "output_text", "text": "MSC"}
                ]
            }]
        }))
        .unwrap();
        assert_eq!(extract_openai_output_text(response).as_deref(), Some("MSC"));
    }

    #[test]
    fn openai_response_without_text_is_rejected() {
        let response: OpenaiResponse = serde_json::from_value(json!({
            "output": [{"type": "reasoning", "content": []}]
        }))
        .unwrap();
        assert_eq!(extract_openai_output_text(response), None);
    }

    #[test]
    fn validate_accepts_3_uppercase_letters() {
        assert_eq!(validate("MSC", &[]), Some("MSC".to_string()));
    }

    #[test]
    fn validate_strips_whitespace_and_quotes() {
        assert_eq!(validate("  'MSC'  \n", &[]), Some("MSC".to_string()));
    }

    #[test]
    fn validate_uppercases_lowercase_input() {
        assert_eq!(validate("msc", &[]), Some("MSC".to_string()));
    }

    #[test]
    fn validate_rejects_wrong_length() {
        assert_eq!(validate("MS", &[]), None);
        assert_eq!(validate("MSCC", &[]), None);
    }

    #[test]
    fn validate_rejects_collision() {
        assert_eq!(validate("MSC", &["MSC".to_string()]), None);
        // Case-insensitive collision check.
        assert_eq!(validate("MSC", &["msc".to_string()]), None);
    }

    #[test]
    fn deterministic_basic_consonant_walk() {
        // First letter + next 2 consonants, in order of appearance.
        // (A model usually picks nicer codes like "MSC" for mission-control;
        // this deterministic fallback skips repeats so produces "MSN".)
        let p = deterministic_prefix("mission-control", &[]);
        assert_eq!(p.len(), 3);
        assert!(p.chars().all(|c| c.is_ascii_uppercase()));
        assert!(p.starts_with('M'));
        // mc-tui → M, C, T
        assert_eq!(deterministic_prefix("mc-tui", &[]), "MCT");
        // blin-agents → B, L, N
        assert_eq!(deterministic_prefix("blin-agents", &[]), "BLN");
    }

    #[test]
    fn deterministic_pads_when_no_consonants() {
        assert_eq!(deterministic_prefix("aei", &[]), "AEI");
        // "a" alone — pad to AXX.
        assert_eq!(deterministic_prefix("a", &[]), "AXX");
    }

    #[test]
    fn deterministic_dedups_with_digit_suffix() {
        // Whatever the natural pick is, taking it should bump to the
        // 2-suffix form on collision.
        let natural = deterministic_prefix("mc-tui", &[]);
        let expected = format!("{}2", natural);
        assert_eq!(
            deterministic_prefix("mc-tui", std::slice::from_ref(&natural)),
            expected
        );
    }
}
