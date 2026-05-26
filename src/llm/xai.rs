//! One-shot xAI Grok client for short string generations.
//!
//! Today's only caller is workspace goal-prefix generation
//! (`MSC` for `mission-control`). The xAI API is OpenAI-compatible at
//! `https://api.x.ai/v1`, so we use the chat completions shape with a
//! tiny prompt and `max_tokens: 8` to keep the round-trip cheap.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const XAI_ENDPOINT: &str = "https://api.x.ai/v1/chat/completions";
/// Default xAI model. Their lineup shifts; this name is the current
/// "small + fast" tier (verified live 2026-05-25 against
/// https://api.x.ai/v1/models).
const DEFAULT_MODEL: &str = "grok-4-fast-non-reasoning";

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

/// Generate a 3-letter uppercase prefix for a workspace name. Avoids any
/// prefix in `used_prefixes` (the caller passes the set of prefixes already
/// assigned to other workspaces this session).
///
/// On success returns the validated 3-letter code. On any failure (network,
/// invalid output, second-try collision) returns an error; the caller is
/// expected to fall through to the deterministic helper.
pub async fn generate_workspace_prefix(
    api_key: &str,
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
            Some(bad) => format!(" Also avoid '{}' (you suggested that and it was rejected).", bad),
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
    let raw = call_xai(api_key, &attempt(None)).await?;
    if let Some(p) = validate(&raw, used_prefixes) {
        return Ok(p);
    }

    // Second try, telling the model what it just got wrong.
    let raw2 = call_xai(api_key, &attempt(Some(&raw))).await?;
    if let Some(p) = validate(&raw2, used_prefixes) {
        return Ok(p);
    }

    Err(anyhow!(
        "xAI returned invalid prefixes twice (last raw output: {:?})",
        raw2.chars().take(40).collect::<String>()
    ))
}

async fn call_xai(api_key: &str, user_prompt: &str) -> Result<String> {
    let body = Request {
        model: DEFAULT_MODEL,
        messages: vec![Message {
            role: "user",
            content: user_prompt,
        }],
        temperature: 0.0,
        max_tokens: 8,
    };

    let client = reqwest::Client::new();
    let resp = client
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
    let parsed: Response = resp.json().await.context("xAI parse")?;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("xAI returned no choices"))?;
    Ok(choice.message.content)
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
/// code (or a digit-suffixed code on collision). Used when xAI is
/// unavailable or returns garbage twice.
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
        // (xAI usually picks nicer codes like "MSC" for mission-control;
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
            deterministic_prefix("mc-tui", &[natural.clone()]),
            expected
        );
    }
}
