//! Per-session record file (design §4), capped to fit the arcmux mesh payload.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::artifacts::Artifact;
use crate::mc_data::paths::data_root;

pub const MAX_FILE_BYTES: usize = 40 * 1024;
const MAX_TEXT_CHARS: usize = 300;
const MAX_AGENT_TURNS: usize = 200;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFile {
    pub host: String,
    pub session: String,
    pub agent: String,
    pub transcript: PathBuf,
    pub workspace: Option<String>,
    pub surface: Option<String>,
    pub status: Option<String>,
    pub segments: Vec<Segment>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub segment: String,
    pub intent: String,
    pub latest: String,
    pub latest_at: Option<DateTime<Utc>>,
    pub state: SegState,
    pub needs_me: Option<String>,
    pub user_inputs: Vec<UserInput>,
    pub agent_turns: Vec<AgentTurn>,
    pub artifacts: Vec<Artifact>,
    pub last_refine_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegState {
    Working,
    WaitingOnMe,
    Blocked,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInput {
    pub at: DateTime<Utc>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentTurn {
    pub at: DateTime<Utc>,
    #[serde(rename = "loop")]
    pub injected: bool,
}

pub fn updates_dir() -> PathBuf {
    data_root().join("updates").join("local")
}

pub fn load(dir: &Path, session: &str) -> Option<SessionFile> {
    let bytes = std::fs::read(dir.join(format!("{session}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomic write (dot-prefixed tmp + rename, so listers never see a partial file).
/// Trims a copy to `MAX_FILE_BYTES`; errors if it still cannot fit.
pub fn save(dir: &Path, file: &SessionFile) -> io::Result<()> {
    let bytes = trimmed_bytes(file)?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.json.tmp", file.session));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dir.join(format!("{}.json", file.session)))
}

fn truncate(s: &mut String) {
    if let Some((i, _)) = s.char_indices().nth(MAX_TEXT_CHARS) {
        s.truncate(i);
    }
}

fn encode(file: &SessionFile) -> io::Result<Vec<u8>> {
    serde_json::to_vec(file).map_err(io::Error::other)
}

fn trimmed_bytes(file: &SessionFile) -> io::Result<Vec<u8>> {
    let bytes = encode(file)?;
    if bytes.len() <= MAX_FILE_BYTES {
        return Ok(bytes);
    }
    let mut f = file.clone();
    for seg in &mut f.segments {
        truncate(&mut seg.intent);
        truncate(&mut seg.latest);
        seg.needs_me.iter_mut().for_each(truncate);
        seg.user_inputs
            .iter_mut()
            .for_each(|u| truncate(&mut u.text));
        let excess = seg.agent_turns.len().saturating_sub(MAX_AGENT_TURNS);
        seg.agent_turns.drain(..excess);
    }
    let mut len = encode(&f)?.len();
    while len > MAX_FILE_BYTES {
        // Oldest input among segments that still have more than one.
        let oldest = f
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.user_inputs.len() > 1)
            .filter_map(|(i, s)| s.user_inputs.first().map(|u| (u.at, i)))
            .min();
        let Some((_, i)) = oldest else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("session record is {len} bytes after trimming"),
            ));
        };
        let dropped = f.segments[i].user_inputs.remove(0);
        len -= encode_len(&dropped)? + 1; // + array comma
    }
    encode(&f)
}

fn encode_len<T: Serialize>(v: &T) -> io::Result<usize> {
    Ok(serde_json::to_vec(v).map_err(io::Error::other)?.len())
}
