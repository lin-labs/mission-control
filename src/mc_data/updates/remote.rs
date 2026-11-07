//! Devbox update records pulled over the local arcmux mesh route.

use super::record::{updates_dir, write_private};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

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
        }
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
