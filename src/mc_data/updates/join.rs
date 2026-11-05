//! Joins an agent session to its cmux surface/workspace: own env, then its
//! tmux client's env, then (devbox) a mosh-port hint.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub pid: u32,
    pub session_id: String,
    pub tmux: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmuxIds {
    pub surface: String,
    pub workspace: String,
}

/// One `hints.json` entry, written by arcmux.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceHint {
    pub mosh_port: u16,
    pub surface_id: String,
    pub workspace_id: String,
}

pub trait Sys {
    fn env_of(&self, pid: u32) -> Option<HashMap<String, String>>;
    /// `tmux -L arcmux list-clients -F '#{client_pid} #{client_session} #{client_tty}'`.
    fn tmux_clients(&self) -> String;
    /// `ss -ulpn` output.
    fn udp_listeners(&self) -> String;
    fn tty_of(&self, pid: u32) -> Option<String>;
}

/// Reads `<pid>.json` files in `dir`, keyed by session id; other files are ignored.
pub fn load_agent_sessions(dir: &Path) -> HashMap<String, AgentSession> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(file_pid) = stem.parse::<u32>() else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(sid) = v["sessionId"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let text = |k: &str| v[k].as_str().map(str::to_string);
        let pid = v["pid"].as_u64().map_or(file_pid, |p| p as u32);
        out.insert(
            sid.to_string(),
            AgentSession {
                pid,
                session_id: sid.to_string(),
                tmux: text("tmux"),
                status: text("status"),
            },
        );
    }
    out
}

pub fn parse_environ(raw: &[u8]) -> HashMap<String, String> {
    raw.split(|b| *b == 0)
        .filter_map(|kv| {
            let s = String::from_utf8_lossy(kv);
            let (k, v) = s.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Parses `ps eww -o command=` output: the command, then ` KEY=VALUE` pairs.
/// Only tokens shaped like `[A-Z_][A-Z0-9_]*=` count; a value containing
/// spaces loses its tail, which is fine for the IDs we need.
pub fn parse_ps_eww(raw: &str) -> HashMap<String, String> {
    let is_key = |k: &str| {
        let mut c = k.chars();
        c.next().is_some_and(|f| f.is_ascii_uppercase() || f == '_')
            && c.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    };
    raw.split_whitespace()
        .filter_map(|t| t.split_once('='))
        .filter(|(k, _)| is_key(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn ids_from(env: &HashMap<String, String>) -> Option<CmuxIds> {
    let get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
    Some(CmuxIds {
        surface: get("CMUX_SURFACE_ID")?,
        workspace: get("CMUX_WORKSPACE_ID")?,
    })
}

/// `(client_pid, client_session, client_tty)` rows.
fn clients(sys: &dyn Sys) -> Vec<(u32, String, String)> {
    sys.tmux_clients()
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((
                f.next()?.parse().ok()?,
                f.next()?.to_string(),
                f.next()?.to_string(),
            ))
        })
        .collect()
}

/// Pid of the `mosh-server` listening on UDP `port` in `ss -ulpn` output.
fn mosh_pid(ss: &str, port: u16) -> Option<u32> {
    let suffix = format!(":{port}");
    ss.lines()
        .filter(|l| l.contains("mosh-server"))
        .find_map(|l| {
            let local = l.split_whitespace().nth(3)?;
            if !local.ends_with(&suffix) {
                return None;
            }
            l.split("pid=")
                .nth(1)?
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
}

pub fn resolve(s: &AgentSession, sys: &dyn Sys, hints: &[SurfaceHint]) -> Option<CmuxIds> {
    if let Some(ids) = sys.env_of(s.pid).as_ref().and_then(ids_from) {
        return Some(ids);
    }
    let session = s
        .tmux
        .as_deref()?
        .split(':')
        .next()
        .filter(|n| !n.is_empty())?;
    let all = clients(sys);
    if let Some(ids) = all
        .iter()
        .filter(|c| c.1 == session)
        .find_map(|c| sys.env_of(c.0).as_ref().and_then(ids_from))
    {
        return Some(ids);
    }
    let ss = sys.udp_listeners();
    hints.iter().find_map(|h| {
        if h.surface_id.is_empty() || h.workspace_id.is_empty() {
            return None;
        }
        let tty = sys.tty_of(mosh_pid(&ss, h.mosh_port)?)?;
        all.iter()
            .any(|c| c.2 == tty && c.1 == session)
            .then(|| CmuxIds {
                surface: h.surface_id.clone(),
                workspace: h.workspace_id.clone(),
            })
    })
}

pub struct RealSys;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Sys for RealSys {
    fn env_of(&self, pid: u32) -> Option<HashMap<String, String>> {
        if let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) {
            return Some(parse_environ(&raw));
        }
        let out = run("ps", &["eww", "-o", "command=", "-p", &pid.to_string()])?;
        (!out.trim().is_empty()).then(|| parse_ps_eww(&out))
    }
    fn tmux_clients(&self) -> String {
        run(
            "tmux",
            &[
                "-L",
                "arcmux",
                "list-clients",
                "-F",
                "#{client_pid} #{client_session} #{client_tty}",
            ],
        )
        .unwrap_or_default()
    }
    fn udp_listeners(&self) -> String {
        run("ss", &["-ulpn"]).unwrap_or_default()
    }
    fn tty_of(&self, pid: u32) -> Option<String> {
        ["0", "1"].iter().find_map(|fd| {
            let p = std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()?;
            let p = p.to_string_lossy().into_owned();
            p.starts_with("/dev/pts/").then_some(p)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        env: HashMap<u32, HashMap<String, String>>,
        clients: String,
        ss: String,
        tty: HashMap<u32, String>,
    }
    impl Sys for Fake {
        fn env_of(&self, pid: u32) -> Option<HashMap<String, String>> {
            self.env.get(&pid).cloned()
        }
        fn tmux_clients(&self) -> String {
            self.clients.clone()
        }
        fn udp_listeners(&self) -> String {
            self.ss.clone()
        }
        fn tty_of(&self, pid: u32) -> Option<String> {
            self.tty.get(&pid).cloned()
        }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn ids_env(s: &str, w: &str) -> HashMap<String, String> {
        env(&[("CMUX_SURFACE_ID", s), ("CMUX_WORKSPACE_ID", w)])
    }
    fn sess() -> AgentSession {
        AgentSession {
            pid: 10,
            session_id: "sid".into(),
            tmux: Some("blin-claude-x-1:@10.%10".into()),
            status: Some("busy".into()),
        }
    }
    fn ids(s: &str, w: &str) -> Option<CmuxIds> {
        Some(CmuxIds {
            surface: s.into(),
            workspace: w.into(),
        })
    }

    #[test]
    fn own_env_wins() {
        let mut f = Fake::default();
        f.env.insert(10, ids_env("S1", "W1"));
        assert_eq!(resolve(&sess(), &f, &[]), ids("S1", "W1"));
    }

    #[test]
    fn partial_own_env_falls_through_to_tmux_client() {
        let mut f = Fake::default();
        f.env.insert(10, env(&[("CMUX_SURFACE_ID", "S1")]));
        f.env.insert(20, ids_env("S2", "W2"));
        f.clients = "30 other /dev/pts/1\n20 blin-claude-x-1 /dev/pts/2\n".into();
        assert_eq!(resolve(&sess(), &f, &[]), ids("S2", "W2"));
    }

    #[test]
    fn mosh_hint_via_pty() {
        let mut f = Fake::default();
        f.clients = "20 blin-claude-x-1 /dev/pts/7\n".into();
        f.ss = "State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
                UNCONN 0 0 0.0.0.0:60033 0.0.0.0:* users:((\"mosh-server\",pid=123,fd=4))\n\
                UNCONN 0 0 *:60034 *:* users:((\"mosh-server\",pid=124,fd=4))\n"
            .into();
        f.tty.insert(123, "/dev/pts/7".into());
        f.tty.insert(124, "/dev/pts/8".into());
        let hint = |p, s: &str| SurfaceHint {
            mosh_port: p,
            surface_id: s.into(),
            workspace_id: "W3".into(),
        };
        let hints = [hint(60034, "S-other"), hint(60033, "S3")];
        assert_eq!(resolve(&sess(), &f, &hints), ids("S3", "W3"));
    }

    #[test]
    fn none_found_is_none() {
        let mut f = Fake::default();
        f.clients = "20 blin-claude-x-1 /dev/pts/2\n".into();
        f.env.insert(20, env(&[("CMUX_WORKSPACE_ID", "W")]));
        assert_eq!(resolve(&sess(), &f, &[]), None);
        let mut s = sess();
        s.tmux = None;
        assert_eq!(resolve(&s, &f, &[]), None);
    }

    #[test]
    fn parses_real_ps_eww_line() {
        let line = "tmux -L arcmux attach -t blin-claude-tools-17794 TERM=xterm-256color \
SHELL=/bin/zsh CMUX_WORKSPACE_ID=2EC28B75-6BE2-46F1-B3F2-0C9C8E1FA273 LANG=en_US.UTF-8 \
CMUX_SURFACE_ID=E35B477C-7711-437E-A430-69972174B67D\n";
        let e = parse_ps_eww(line);
        assert_eq!(e["CMUX_SURFACE_ID"], "E35B477C-7711-437E-A430-69972174B67D");
        assert_eq!(
            e["CMUX_WORKSPACE_ID"],
            "2EC28B75-6BE2-46F1-B3F2-0C9C8E1FA273"
        );
        assert_eq!(e["TERM"], "xterm-256color");
        assert!(!e.contains_key("-L") && !e.contains_key("tmux"));
    }

    #[test]
    fn parses_environ_and_sessions_dir() {
        assert_eq!(parse_environ(b"A=1\0B=x=y\0\0")["B"], "x=y");
        let d = std::env::temp_dir().join(format!("mc-join-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("5.json"),
            r#"{"pid":5,"sessionId":"abc","tmux":"t:@1.%1","status":"busy"}"#,
        )
        .unwrap();
        std::fs::write(d.join("5.deadbeef.key"), "zzz").unwrap();
        let m = load_agent_sessions(&d);
        std::fs::remove_dir_all(&d).ok();
        assert_eq!(m.len(), 1);
        assert_eq!(m["abc"].pid, 5);
        assert_eq!(m["abc"].tmux.as_deref(), Some("t:@1.%1"));
    }
}
