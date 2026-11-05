//! `mission-control watch`: every tick, read the new bytes of grown agent
//! transcripts, apply them to per-session records, and write a heartbeat.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::engine::{self, EngineOpts};
use super::join::{self, RealSys, Snapshot, SurfaceHint};
use super::judge::Judge;
use super::record::{self, SessionFile};
use super::tail::{self, ReadOutcome, Turn};
use crate::mc_data::surface_kind::SurfaceKind;

const SEED_HOURS: i64 = 24;
const MAX_WARNING_CHARS: usize = 160;
const CODEX_MAX_DEPTH: usize = 6;

pub struct WatchOpts {
    pub interval: u64,
    pub once: bool,
    pub seed_only: bool,
    pub updates_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Offset {
    offset: u64,
    len: u64,
    mtime: DateTime<Utc>,
}

/// Stands in when there is no Beam client: every call fails, so routing
/// falls back to the current segment and state is left as is.
struct NoJudge;

#[async_trait]
impl Judge for NoJudge {
    async fn ask(&self, _prompt: &str, _max_tokens: u32) -> Result<String> {
        bail!("no Beam client")
    }
}

pub async fn run(judge: Option<Arc<dyn Judge>>, opts: WatchOpts) -> Result<()> {
    let host = hostname();
    loop {
        let result = tick(judge.as_deref(), &opts, &host).await;
        if opts.once {
            return result;
        }
        if let Err(e) = result {
            eprintln!("watch: tick failed: {e:#}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(opts.interval)).await;
    }
}

async fn tick(judge: Option<&dyn Judge>, opts: &WatchOpts, host: &str) -> Result<()> {
    let started = Instant::now();
    let now = Utc::now();
    let dir = &opts.updates_dir;
    let offsets_path = dir.join(".offsets.json");
    let prev: Option<BTreeMap<PathBuf, Offset>> = std::fs::read(&offsets_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let first_run = prev.is_none();
    let prev = prev.unwrap_or_default();
    let seed = first_run || opts.seed_only;

    let mut offsets = BTreeMap::new();
    let mut changed = Vec::new();
    for (path, kind) in transcripts() {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let (len, mtime) = (meta.len(), meta.modified().map_or(now, DateTime::from));
        match prev.get(&path) {
            Some(o) if (o.len, o.mtime) == (len, mtime) => {
                offsets.insert(path, *o);
            }
            None if first_run && now - mtime > Duration::hours(SEED_HOURS) => {
                offsets.insert(
                    path,
                    Offset {
                        offset: len,
                        len,
                        mtime,
                    },
                );
            }
            o => changed.push((path, kind, o.map_or(0, |o| o.offset), len, mtime)),
        }
    }

    let mut warnings = Vec::new();
    let mut written = 0;
    if !changed.is_empty() {
        let home = dirs::home_dir().unwrap_or_default();
        let sessions = join::load_agent_sessions(&home.join(".claude/sessions"));
        let hints: Vec<SurfaceHint> = std::fs::read(dir.join("hints.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // One host snapshot per tick, only when some session can be joined.
        let joins: Vec<_> = {
            let snap = (!sessions.is_empty()).then(|| Snapshot::new(&RealSys));
            changed
                .iter()
                .map(|(path, kind, ..)| {
                    let s = (*kind == SurfaceKind::Claude)
                        .then(|| sessions.get(&session_id(path)))
                        .flatten()?;
                    let ids = join::resolve(s, snap.as_ref()?, &hints);
                    Some((s.status.clone(), ids))
                })
                .collect()
        };

        for ((path, kind, from, len, mtime), joined) in changed.into_iter().zip(joins) {
            let mut out = match tail::read_new(kind, &path, from, mtime) {
                Ok(out) => out,
                Err(e) => {
                    warnings.push(format!("read {}: {e}", path.display()));
                    // Keep the old entry so the next tick retries from it.
                    if let Some(o) = prev.get(&path) {
                        offsets.insert(path, *o);
                    }
                    continue;
                }
            };
            let session = session_id(&path);
            let mut file = record::load(dir, &session).unwrap_or_else(|| SessionFile {
                host: host.to_string(),
                session: session.clone(),
                agent: match kind {
                    SurfaceKind::Codex => "codex",
                    _ => "claude",
                }
                .into(),
                transcript: path.clone(),
                workspace: None,
                surface: None,
                status: None,
                segments: vec![],
                updated_at: now,
            });
            if out.reset {
                drop_seen(&file, &mut out);
            }
            let (status, ids) = joined.unwrap_or_default();
            file.workspace = ids.as_ref().map(|i| i.workspace.clone());
            file.surface = ids.map(|i| i.surface);
            let agent_busy = status.as_deref().map(|s| s == "busy");
            file.status = status;
            let w = engine::apply(
                judge.unwrap_or(&NoJudge),
                &mut file,
                &out,
                agent_busy,
                EngineOpts { seed, now },
            )
            .await;
            if judge.is_some() {
                warnings.extend(w.into_iter().map(|w| format!("{session}: {w}")));
            }
            // Sessions with no user input yet (e.g. automation) get no record.
            if !file.segments.is_empty() {
                file.updated_at = now;
                match record::save(dir, &file) {
                    Ok(()) => written += 1,
                    Err(e) => warnings.push(format!("save {session}: {e}")),
                }
            }
            let offset = out.next_offset;
            offsets.insert(path, Offset { offset, len, mtime });
        }
    }

    std::fs::create_dir_all(dir)?;
    write_atomic(&offsets_path, &serde_json::to_vec(&offsets)?)?;
    let monitor = serde_json::json!({
        "host": host,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "last_tick": now,
    });
    write_atomic(&dir.join("monitor.json"), &serde_json::to_vec(&monitor)?)?;

    if written > 0 || !warnings.is_empty() {
        eprintln!(
            "watch: {written} records written, {} warnings, {} ms",
            warnings.len(),
            started.elapsed().as_millis()
        );
        for w in &warnings {
            eprintln!(
                "watch:   {}",
                w.chars().take(MAX_WARNING_CHARS).collect::<String>()
            );
        }
    }
    Ok(())
}

/// Top-level Claude transcripts (`projects/<project>/<id>.jsonl`; subagent
/// transcripts sit deeper) and Codex `rollout-*.jsonl` files.
fn transcripts() -> Vec<(PathBuf, SurfaceKind)> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut out = Vec::new();
    for project in subdirs(&home.join(".claude/projects")) {
        for f in files(&project) {
            if f.extension().is_some_and(|e| e == "jsonl") {
                out.push((f, SurfaceKind::Claude));
            }
        }
    }
    let mut stack = vec![(home.join(".codex/sessions"), 0)];
    while let Some((d, depth)) = stack.pop() {
        for f in files(&d) {
            let name = f.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                out.push((f, SurfaceKind::Codex));
            }
        }
        if depth < CODEX_MAX_DEPTH {
            stack.extend(subdirs(&d).into_iter().map(|s| (s, depth + 1)));
        }
    }
    out
}

/// Entries of `dir` (following symlinks) that pass `keep`.
fn entries(dir: &Path, keep: fn(&std::fs::Metadata) -> bool) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| keep(&m)))
        .collect()
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    entries(dir, |m| m.is_dir())
}

fn files(dir: &Path) -> Vec<PathBuf> {
    entries(dir, |m| m.is_file())
}

/// Transcript file stem: the Claude session UUID or the Codex rollout name.
fn session_id(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// After a reset the file is re-read from 0: keep only what is newer than
/// anything the record already holds.
fn drop_seen(file: &SessionFile, out: &mut ReadOutcome) {
    let newest = file
        .segments
        .iter()
        .flat_map(|s| {
            let users = s.user_inputs.iter().map(|u| u.at);
            let agents = s.agent_turns.iter().map(|a| a.at);
            users.chain(agents).chain(s.artifacts.iter().map(|a| a.at))
        })
        .max();
    let Some(newest) = newest else { return };
    out.turns.retain(|t| match t {
        Turn::User { at, .. } | Turn::Agent { at, .. } => *at > newest,
    });
    out.lines.retain(|l| {
        l.get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|t| t > newest)
    });
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".into())
}
