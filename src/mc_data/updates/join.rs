//! Joins an agent session to its cmux surface/workspace: own env, then its
//! tmux client's env, then (devbox) a mosh-port hint.

use std::cell::{OnceCell, RefCell};
use std::cmp::Reverse;
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
    /// `tmux -L arcmux list-clients -F '#{client_pid} #{client_session} #{client_tty} #{client_activity}'`.
    fn tmux_clients(&self) -> String;
    /// `ss -ulpn` output.
    fn udp_listeners(&self) -> String;
    fn tty_of(&self, pid: u32) -> Option<String>;
    fn children_of(&self, pid: u32) -> Vec<u32>;
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

/// Keeps only `CMUX_*` keys: other vars may be secrets.
pub fn parse_environ(raw: &[u8]) -> HashMap<String, String> {
    raw.split(|b| *b == 0)
        .filter_map(|kv| {
            let s = String::from_utf8_lossy(kv);
            let (k, v) = s.split_once('=')?;
            k.starts_with("CMUX_")
                .then(|| (k.to_string(), v.to_string()))
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
        .filter(|(k, _)| is_key(k) && k.starts_with("CMUX_"))
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

struct Client {
    pid: u32,
    session: String,
    tty: String,
    activity: u64,
}

/// Per-tick view of the host: built once, shared by every `resolve` call.
pub struct Snapshot<'a> {
    sys: &'a dyn Sys,
    clients: Vec<Client>,
    udp: OnceCell<String>,
    env: RefCell<HashMap<u32, Option<CmuxIds>>>,
}

impl<'a> Snapshot<'a> {
    pub fn new(sys: &'a dyn Sys) -> Self {
        let clients = sys
            .tmux_clients()
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                Some(Client {
                    pid: f.next()?.parse().ok()?,
                    session: f.next()?.to_string(),
                    tty: f.next()?.to_string(),
                    activity: f.next().and_then(|a| a.parse().ok()).unwrap_or(0),
                })
            })
            .collect();
        Snapshot {
            sys,
            clients,
            udp: OnceCell::new(),
            env: RefCell::default(),
        }
    }

    fn ids(&self, pid: u32) -> Option<CmuxIds> {
        let mut memo = self.env.borrow_mut();
        memo.entry(pid)
            .or_insert_with(|| self.sys.env_of(pid).as_ref().and_then(ids_from))
            .clone()
    }
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

pub fn resolve(s: &AgentSession, snap: &Snapshot, hints: &[SurfaceHint]) -> Option<CmuxIds> {
    if let Some(ids) = snap.ids(s.pid) {
        return Some(ids);
    }
    let session = s
        .tmux
        .as_deref()?
        .split(':')
        .next()
        .filter(|n| !n.is_empty())?;
    // Newest-active client with both IDs; ties go to the lowest pid.
    let best = snap
        .clients
        .iter()
        .filter(|c| c.session == session)
        .filter_map(|c| Some(((Reverse(c.activity), c.pid), snap.ids(c.pid)?)))
        .min_by_key(|(k, _)| *k);
    if let Some((_, ids)) = best {
        return Some(ids);
    }
    if hints.is_empty() {
        return None;
    }
    let ss = snap.udp.get_or_init(|| snap.sys.udp_listeners());
    hints.iter().find_map(|h| {
        if h.surface_id.is_empty() || h.workspace_id.is_empty() {
            return None;
        }
        // The pty belongs to mosh-server's child; its own fds are /dev/null.
        let server = mosh_pid(ss, h.mosh_port)?;
        let ttys: Vec<String> = snap
            .sys
            .children_of(server)
            .into_iter()
            .filter_map(|p| snap.sys.tty_of(p))
            .collect();
        snap.clients
            .iter()
            .any(|c| c.session == session && ttys.contains(&c.tty))
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
                "#{client_pid} #{client_session} #{client_tty} #{client_activity}",
            ],
        )
        .unwrap_or_default()
    }
    fn udp_listeners(&self) -> String {
        run("ss", &["-ulpn"]).unwrap_or_default()
    }
    fn children_of(&self, pid: u32) -> Vec<u32> {
        let ppid_of = |p: u32| -> Option<u32> {
            let stat = std::fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
            stat.rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        };
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return vec![];
        };
        rd.flatten()
            .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
            .filter(|p| ppid_of(*p) == Some(pid))
            .collect()
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
        kids: HashMap<u32, Vec<u32>>,
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
        fn children_of(&self, pid: u32) -> Vec<u32> {
            self.kids.get(&pid).cloned().unwrap_or_default()
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
        assert_eq!(resolve(&sess(), &Snapshot::new(&f), &[]), ids("S1", "W1"));
    }

    #[test]
    fn partial_own_env_falls_through_to_tmux_client() {
        let mut f = Fake::default();
        f.env.insert(10, env(&[("CMUX_SURFACE_ID", "S1")]));
        f.env.insert(20, ids_env("S2", "W2"));
        f.clients = "30 other /dev/pts/1\n20 blin-claude-x-1 /dev/pts/2\n".into();
        assert_eq!(resolve(&sess(), &Snapshot::new(&f), &[]), ids("S2", "W2"));
    }

    #[test]
    fn mosh_hint_via_child_pty() {
        let mut f = Fake::default();
        f.clients = "20 blin-claude-x-1 /dev/pts/7 100\n".into();
        f.ss = "State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
UNCONN 0      0          100.85.14.76:60001      0.0.0.0:*    users:((\"mosh-server\",pid=452490,fd=4)) \n\
UNCONN 0 0 *:60034 *:* users:((\"mosh-server\",pid=124,fd=4))\n"
            .into();
        // Real mosh-server fds are /dev/null; only the child has the pts.
        f.kids.insert(452490, vec![452491]);
        f.tty.insert(452491, "/dev/pts/7".into());
        f.kids.insert(124, vec![125]);
        f.tty.insert(125, "/dev/pts/8".into());
        let hint = |p, s: &str| SurfaceHint {
            mosh_port: p,
            surface_id: s.into(),
            workspace_id: "W3".into(),
        };
        let hints = [hint(60034, "S-other"), hint(60001, "S3")];
        assert_eq!(
            resolve(&sess(), &Snapshot::new(&f), &hints),
            ids("S3", "W3")
        );
    }

    #[test]
    fn newest_active_client_with_ids_wins_ties_lowest_pid() {
        let mut f = Fake::default();
        f.env.insert(21, ids_env("S-old", "W-old"));
        f.env.insert(22, ids_env("S-new", "W-new"));
        f.env.insert(23, ids_env("S-tie", "W-tie"));
        // 24 is newest but has no IDs; 23 ties with 22 on activity.
        f.clients = "21 blin-claude-x-1 /dev/pts/1 10\n23 blin-claude-x-1 /dev/pts/3 50\n\
22 blin-claude-x-1 /dev/pts/2 50\n24 blin-claude-x-1 /dev/pts/4 99\n"
            .into();
        assert_eq!(
            resolve(&sess(), &Snapshot::new(&f), &[]),
            ids("S-new", "W-new")
        );
    }

    #[test]
    fn parsers_drop_non_cmux_vars() {
        let e = parse_environ(b"SECRET=x\0CMUX_SURFACE_ID=y");
        assert_eq!((e.len(), e["CMUX_SURFACE_ID"].as_str()), (1, "y"));
        let e = parse_ps_eww("cmd API_KEY=x CMUX_SURFACE_ID=y");
        assert_eq!((e.len(), e["CMUX_SURFACE_ID"].as_str()), (1, "y"));
    }

    #[test]
    fn none_found_is_none() {
        let mut f = Fake::default();
        f.clients = "20 blin-claude-x-1 /dev/pts/2\n".into();
        f.env.insert(20, env(&[("CMUX_WORKSPACE_ID", "W")]));
        assert_eq!(resolve(&sess(), &Snapshot::new(&f), &[]), None);
        let mut s = sess();
        s.tmux = None;
        assert_eq!(resolve(&s, &Snapshot::new(&f), &[]), None);
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
        assert!(!e.contains_key("-L") && !e.contains_key("tmux"));
    }

    #[test]
    fn parses_environ_and_sessions_dir() {
        assert_eq!(parse_environ(b"A=1\0CMUX_B=x=y\0\0")["CMUX_B"], "x=y");
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
