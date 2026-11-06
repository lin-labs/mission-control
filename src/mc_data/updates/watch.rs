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
/// Bytes read and applied per step, bounding memory on huge transcripts.
pub const CHUNK_BYTES: u64 = 4 << 20;
/// An unknown transcript is seeded from at most its last this many bytes.
pub const SEED_BYTES: u64 = 8 << 20;

pub struct WatchOpts {
    pub interval: u64,
    pub once: bool,
    pub seed_only: bool,
    pub updates_dir: PathBuf,
    pub chunk_bytes: u64,
    pub seed_bytes: u64,
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
    std::fs::create_dir_all(dir)?;
    write_monitor(dir, host, now)?;

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
            // Unknown and old (first run, or offsets lost): only read growth.
            None if now - mtime > Duration::hours(SEED_HOURS) => {
                offsets.insert(
                    path,
                    Offset {
                        offset: len,
                        len,
                        mtime,
                    },
                );
            }
            Some(o) => {
                // Kept until this session's record is saved.
                offsets.insert(path.clone(), *o);
                changed.push((path, kind, o.offset, false, len, mtime));
            }
            None => {
                let tail = len.saturating_sub(opts.seed_bytes);
                let start = tail::line_start_at(&path, tail).unwrap_or(tail);
                changed.push((path, kind, start, true, len, mtime));
            }
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

        for ((path, kind, mut from, unknown, len, mtime), joined) in changed.into_iter().zip(joins)
        {
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
            let (status, ids) = joined.unwrap_or_default();
            file.workspace = ids.as_ref().map(|i| i.workspace.clone());
            file.surface = ids.map(|i| i.surface);
            let agent_busy = status.as_deref().map(|s| s == "busy");
            file.status = status;
            // Replaying history (seed window, or offsets lost): no refines.
            let seed = seed || (unknown && from > 0);
            // A re-read must not re-apply what the record already has.
            let mut rereading = unknown;
            let mut saved = false;
            // One chunk in memory at a time: apply, save, then record the offset.
            loop {
                let mut out = match tail::read_new(kind, &path, from, mtime, opts.chunk_bytes) {
                    Ok(out) => out,
                    Err(e) => {
                        warnings.push(format!("read {}: {e}", path.display()));
                        break;
                    }
                };
                rereading |= out.reset;
                if rereading {
                    drop_seen(&file, &mut out);
                    rereading = out.turns.is_empty();
                }
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
                    if let Err(e) = record::save(dir, &file) {
                        // Keep the old offset so these turns are re-read.
                        warnings.push(format!("save {session}: {e}"));
                        break;
                    }
                    saved = true;
                }
                let done = out.next_offset == from || out.next_offset >= len;
                from = out.next_offset;
                // Until caught up, a `len` that cannot match makes the next tick resume.
                let len = if done { len } else { from };
                offsets.insert(
                    path.clone(),
                    Offset {
                        offset: from,
                        len,
                        mtime,
                    },
                );
                write_atomic(&offsets_path, &serde_json::to_vec(&offsets)?)?;
                if done {
                    break;
                }
            }
            written += saved as usize;
        }
    }

    write_atomic(&offsets_path, &serde_json::to_vec(&offsets)?)?;
    write_monitor(dir, host, Utc::now())?;

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
    for project in list(&home.join(".claude/projects")).0 {
        for f in list(&project).1 {
            if f.extension().is_some_and(|e| e == "jsonl") {
                out.push((f, SurfaceKind::Claude));
            }
        }
    }
    let mut stack = vec![(home.join(".codex/sessions"), 0)];
    while let Some((d, depth)) = stack.pop() {
        let (dirs, files) = list(&d);
        for f in files {
            let name = f.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                out.push((f, SurfaceKind::Codex));
            }
        }
        if depth < CODEX_MAX_DEPTH {
            stack.extend(dirs.into_iter().map(|s| (s, depth + 1)));
        }
    }
    out
}

/// Subdirectories and files of `dir`; only symlinks cost a stat.
fn list(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut dirs, mut files) = (vec![], vec![]);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (dirs, files);
    };
    for e in rd.flatten() {
        let Ok(mut ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_symlink() {
            let Ok(m) = std::fs::metadata(&p) else {
                continue;
            };
            ft = m.file_type();
        }
        if ft.is_dir() {
            dirs.push(p);
        } else if ft.is_file() {
            files.push(p);
        }
    }
    (dirs, files)
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

fn write_monitor(dir: &Path, host: &str, at: DateTime<Utc>) -> Result<()> {
    let monitor = serde_json::json!({
        "host": host,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "last_tick": at,
    });
    Ok(write_atomic(
        &dir.join("monitor.json"),
        &serde_json::to_vec(&monitor)?,
    )?)
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
