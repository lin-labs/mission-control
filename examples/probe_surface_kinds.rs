//! Live probe: shells out to `cmux tree --all --json`, then runs
//! `mc_data::surface_kind::detect` on each surface's tty. Used as the T1
//! live-verification step — confirms that real-world lsof+ps output is
//! correctly classified.

use std::collections::HashMap;
use std::path::PathBuf;

use mission_control::mc_data::surface_kind::{self, SurfaceKind};

#[derive(serde::Deserialize)]
struct Tree {
    windows: Vec<Window>,
}
#[derive(serde::Deserialize)]
struct Window {
    workspaces: Vec<Ws>,
}
#[derive(serde::Deserialize)]
struct Ws {
    #[serde(default)]
    panes: Vec<Pane>,
}
#[derive(serde::Deserialize)]
struct Pane {
    #[serde(default)]
    surfaces: Vec<Surface>,
}
#[derive(serde::Deserialize)]
struct Surface {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    tty: Option<String>,
}

fn main() {
    let sock = std::fs::read_to_string("/tmp/cmux-last-socket-path")
        .expect("read cmux socket path");
    let sock_path = PathBuf::from(sock.trim());

    let out = std::process::Command::new("/Applications/cmux.app/Contents/Resources/bin/cmux")
        .env("CMUX_SOCKET_PATH", &sock_path)
        .args(["tree", "--all", "--json"])
        .output()
        .expect("run cmux tree");

    let tree: Tree = serde_json::from_slice(&out.stdout).expect("parse cmux tree");

    let mut by_kind: HashMap<&str, Vec<String>> = HashMap::new();

    for w in tree.windows {
        for ws in w.workspaces {
            for pane in ws.panes {
                for s in pane.surfaces {
                    let kind = match s.tty.as_deref() {
                        Some(t) if !t.is_empty() => surface_kind::detect(t),
                        _ => SurfaceKind::Unknown,
                    };
                    let label = match kind {
                        SurfaceKind::Claude => "claude",
                        SurfaceKind::Codex => "codex",
                        SurfaceKind::OtherAgent => "other_agent",
                        SurfaceKind::Shell => "shell",
                        SurfaceKind::Remote => "remote",
                        SurfaceKind::Unknown => "unknown",
                    };
                    let title = s.title.chars().take(40).collect::<String>();
                    by_kind.entry(label).or_default().push(format!(
                        "{} tty={} title={:?}",
                        s.ref_id,
                        s.tty.as_deref().unwrap_or(""),
                        title
                    ));
                }
            }
        }
    }

    for kind in ["claude", "codex", "other_agent", "shell", "unknown"] {
        let v = by_kind.get(kind).cloned().unwrap_or_default();
        println!("== {} ({} surfaces) ==", kind, v.len());
        for s in v.iter().take(4) {
            println!("  {}", s);
        }
        if v.len() > 4 {
            println!("  ... and {} more", v.len() - 4);
        }
    }
}
