//! Devbox update records pulled over the local arcmux mesh route.

use super::record::{updates_dir, write_private};
use chrono::{DateTime, Duration, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// `since` slack under the devbox's last tick, on the devbox's own clock.
const SINCE_SLACK_SECS: i64 = 120;

/// Content of the devbox's `monitor.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Monitor {
    pub host: String,
    pub last_tick: String,
    pub pid: u32,
    pub version: String,
}

#[derive(Debug, Default)]
pub struct UpdatesReply {
    pub files: Vec<(String, Value)>,
    pub monitor: Option<Monitor>,
    /// Cursor the fetch stopped at (call cap or a failed later call).
    pub pending_cursor: Option<String>,
}

/// Where the next fetch starts; kept in `updates/<peer>/.since`.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FetchMark {
    pub since: Option<String>,
    pub cursor: Option<String>,
    /// Devbox `last_tick` when the fetch that `cursor` continues began.
    pub start_tick: Option<String>,
}

impl FetchMark {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(".since"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        write_private(&dir.join(".since"), &serde_json::to_vec(self)?)
    }

    /// A pending cursor resumes with the same `since`; a complete fetch
    /// moves `since` to the devbox's last tick, as of the fetch's first
    /// call, minus slack: records changed while it spanned refreshes stay due.
    pub fn next(&self, reply: &UpdatesReply) -> Self {
        let start_tick = match &self.cursor {
            Some(_) => self.start_tick.clone(),
            None => reply.monitor.as_ref().map(|m| m.last_tick.clone()),
        };
        if let Some(c) = &reply.pending_cursor {
            return Self {
                since: self.since.clone(),
                cursor: Some(c.clone()),
                start_tick,
            };
        }
        let since = start_tick
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| {
                (t - Duration::seconds(SINCE_SLACK_SECS)).to_rfc3339_opts(SecondsFormat::Secs, true)
            });
        Self {
            since: since.or_else(|| self.since.clone()),
            cursor: None,
            start_tick: None,
        }
    }
}

#[derive(Deserialize)]
pub struct UpdatesFile {
    pub name: String,
    pub data: Value,
}

#[derive(Deserialize)]
pub struct UpdatesWire {
    #[serde(default)]
    pub files: Vec<UpdatesFile>,
    #[serde(default)]
    pub monitor: Option<Monitor>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// `mosh-client -# … 136.66.96.196 60033` → 60033.
pub fn mosh_port_from_args(cmd: &str) -> Option<u16> {
    let mut words = cmd.split_whitespace();
    words
        .next()
        .filter(|w| w.rsplit('/').next() == Some("mosh-client"))?;
    words.last()?.parse().ok()
}

/// Write `updates/<peer>/<name>` (and `monitor.json`) atomically.
pub fn store(peer: &str, reply: &UpdatesReply) -> std::io::Result<()> {
    if peer.is_empty()
        || peer.contains(['/', '\\'])
        || peer.contains("..")
        || peer.starts_with('.')
        || peer == "local"
    {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    store_in(&updates_dir().with_file_name(peer), reply)
}

fn store_in(dir: &Path, reply: &UpdatesReply) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let write = |name: &str, bytes: Vec<u8>| {
        let tmp = dir.join(format!(".{name}.tmp"));
        write_private(&tmp, &bytes)?;
        std::fs::rename(&tmp, dir.join(name))
    };
    for (name, data) in &reply.files {
        if name.is_empty() || name.contains('/') || name.contains("..") || name.starts_with('.') {
            continue;
        }
        write(name, serde_json::to_vec(data)?)?;
    }
    if let Some(m) = &reply.monitor {
        let m = serde_json::json!({
            "host": m.host, "last_tick": m.last_tick, "pid": m.pid, "version": m.version,
        });
        write("monitor.json", serde_json::to_vec(&m)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_reply_parses_fixture() {
        let wire: UpdatesWire = serde_json::from_str(include_str!(
            "../../../tests/fixtures/arcmux_mesh/updates.json"
        ))
        .unwrap();
        assert_eq!(wire.files.len(), 2);
        assert_eq!(wire.files[0].name, "s1.json");
        assert_eq!(wire.monitor.unwrap().pid, 4242);
        assert_eq!(wire.next_cursor.as_deref(), Some("c2"));
    }

    #[test]
    fn mosh_port_parsed() {
        let c = "mosh-client -# devbox ssh | 136.66.96.196 60033";
        assert_eq!(mosh_port_from_args(c), Some(60033));
        assert_eq!(
            mosh_port_from_args("/usr/bin/mosh-client 1.2.3.4 61000"),
            Some(61000)
        );
    }

    #[test]
    fn non_mosh_command_is_none() {
        assert_eq!(mosh_port_from_args("ssh devbox 22"), None);
        assert_eq!(mosh_port_from_args("mosh-client"), None);
        assert_eq!(mosh_port_from_args(""), None);
    }

    fn reply(names: &[&str]) -> UpdatesReply {
        UpdatesReply {
            files: names
                .iter()
                .map(|n| (n.to_string(), serde_json::json!({"n": n})))
                .collect(),
            monitor: Some(Monitor {
                host: "devbox".into(),
                last_tick: "2026-11-06T00:00:00Z".into(),
                pid: 1,
                version: "1".into(),
            }),
            pending_cursor: None,
        }
    }

    #[test]
    fn mark_advances_only_after_complete_fetch() {
        let start = FetchMark {
            since: Some("t0".into()),
            ..Default::default()
        };
        let mut r = reply(&[]);
        r.pending_cursor = Some("c5".into());
        let pending = start.next(&r);
        assert_eq!(
            pending,
            FetchMark {
                since: Some("t0".into()),
                cursor: Some("c5".into()),
                start_tick: Some("2026-11-06T00:00:00Z".into()),
            }
        );
        r.pending_cursor = None;
        let done = pending.next(&r);
        assert_eq!(
            done,
            FetchMark {
                since: Some("2026-11-05T23:58:00Z".into()),
                ..Default::default()
            }
        );
        let t = tempfile::tempdir().unwrap();
        assert_eq!(FetchMark::load(t.path()), FetchMark::default());
        done.save(t.path()).unwrap();
        assert_eq!(FetchMark::load(t.path()), done);
    }

    #[test]
    fn fetch_spanning_refreshes_takes_since_from_its_first_tick() {
        let mut r = reply(&[]);
        r.pending_cursor = Some("c50".into());
        let mark = FetchMark::default().next(&r);
        // Hours later the resumed fetch completes with a newer tick.
        r.monitor.as_mut().unwrap().last_tick = "2026-11-06T05:00:00Z".into();
        r.pending_cursor = Some("c99".into());
        let mark = mark.next(&r);
        r.pending_cursor = None;
        let done = mark.next(&r);
        assert_eq!(done.since.as_deref(), Some("2026-11-05T23:58:00Z"));
        assert_eq!((done.cursor, done.start_tick), (None, None));
    }

    #[test]
    fn store_replaces_leftover_world_readable_tmp() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("devbox");
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join(".a.json.tmp");
        std::fs::write(&tmp, "old").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        store_in(&dir, &reply(&["a.json"])).unwrap();
        let mode = std::fs::metadata(dir.join("a.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn store_writes_files_under_peer_dir() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("devbox");
        store_in(&dir, &reply(&["a.json"])).unwrap();
        let a: Value = serde_json::from_slice(&std::fs::read(dir.join("a.json")).unwrap()).unwrap();
        assert_eq!(a["n"], "a.json");
        assert!(dir.join("monitor.json").exists());
        assert!(!dir.join(".a.json.tmp").exists());
    }

    #[test]
    fn store_rejects_bad_peer() {
        for p in ["", "a/b", "a\\b", "..", ".x", "local"] {
            let e = store(p, &reply(&["a.json"])).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{p}");
        }
    }

    #[test]
    fn store_rejects_path_names() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("devbox");
        store_in(
            &dir,
            &reply(&["../x.json", "a/b.json", "..", ".h.json", "ok.json"]),
        )
        .unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["monitor.json", "ok.json"]);
        assert!(!t.path().join("x.json").exists());
    }
}
