//! Persistent cache of provider-generated "overall" session summaries.
//!
//! Keyed by the agent's **transcript path** (stable per session, unlike a
//! surface UUID), so a summary survives mc restarts and the 4-turn change gate
//! keeps working across them: we regenerate only when the session's user-turn
//! count has advanced ≥ `OVERALL_SUMMARY_EVERY` since the cached summary, and
//! otherwise reuse the cached line. Stored at
//! `~/data/mission-control/overall-summaries.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mc_data::paths;

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    entries: HashMap<String, Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    summary: String,
    /// User-turn count when this summary was generated (drives the gate).
    turns: usize,
}

fn store_path() -> PathBuf {
    paths::data_root().join("overall-summaries.json")
}

fn key(transcript_path: &Path) -> String {
    transcript_path.to_string_lossy().into_owned()
}

/// Load the whole cache: transcript path -> (summary, turns_at_generation).
pub fn load() -> HashMap<String, (String, usize)> {
    let Ok(raw) = std::fs::read_to_string(store_path()) else {
        return HashMap::new();
    };
    let store: Store = serde_json::from_str(&raw).unwrap_or_default();
    store
        .entries
        .into_iter()
        .map(|(k, e)| (k, (e.summary, e.turns)))
        .collect()
}

/// Look up a single cached summary by transcript path.
pub fn get(transcript_path: &Path) -> Option<(String, usize)> {
    load().get(&key(transcript_path)).cloned()
}

/// Upsert a summary (read-modify-write the whole store; atomic rename).
pub fn put(transcript_path: &Path, summary: &str, turns: usize) -> std::io::Result<()> {
    let path = store_path();
    let mut map = load();
    map.insert(
        key(transcript_path),
        (summary.to_string(), turns),
    );
    let store = Store {
        entries: map
            .into_iter()
            .map(|(k, (summary, turns))| (k, Entry { summary, turns }))
            .collect(),
    };
    let body = serde_json::to_string(&store).unwrap_or_default();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_via_store_struct() {
        // Pure serialization roundtrip (no real FS dependence).
        let mut map: HashMap<String, (String, usize)> = HashMap::new();
        map.insert("/t/a.jsonl".into(), ("build the thing".into(), 8));
        let store = Store {
            entries: map
                .iter()
                .map(|(k, (s, t))| (k.clone(), Entry { summary: s.clone(), turns: *t }))
                .collect(),
        };
        let body = serde_json::to_string(&store).unwrap();
        let back: Store = serde_json::from_str(&body).unwrap();
        let e = back.entries.get("/t/a.jsonl").unwrap();
        assert_eq!(e.summary, "build the thing");
        assert_eq!(e.turns, 8);
    }
}
