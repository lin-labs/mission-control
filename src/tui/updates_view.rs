//! Updates screen (`u`): what each agent session is doing, newest first.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::mc_data::arcmux_mesh::ArcmuxMeshClient;
use crate::mc_data::updates::join::SurfaceHint;
use crate::mc_data::updates::record::{self, SegState, SessionFile, updates_dir};
use crate::mc_data::updates::remote::{self, mosh_port_from_args};
use crate::mc_data::updates::view::{Entry, Grouping, Order, Row, build_rows};
use crate::tui::app::WorkspaceState;

const PEER: &str = "devbox";
const STALE_SECS: i64 = 120;

pub struct UpdatesScreen {
    pub order: Order,
    pub grouping: Grouping,
    pub selected: usize,
    pub expanded: HashSet<String>,
    pub rows: Vec<Row>,
    pub remote: RemoteHealth,
    files: Vec<SessionFile>,
    names: HashMap<String, String>,
    loaded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteHealth {
    Fresh,
    /// `since_secs` is `u64::MAX` when the devbox reported no heartbeat at all.
    Stale {
        since_secs: u64,
    },
    Unreachable,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UpdatesAction {
    None,
    Close,
    Select {
        workspace: String,
        surface: Option<String>,
    },
    Refresh,
}

/// One refresh's result, gathered off the UI thread by [`gather`].
pub struct UpdatesLoad {
    pub files: Vec<SessionFile>,
    pub last_tick: Option<DateTime<Utc>>,
    pub fetch_ok: bool,
}

pub fn health(
    monitor_last_tick: Option<DateTime<Utc>>,
    fetch_ok: bool,
    now: DateTime<Utc>,
) -> RemoteHealth {
    if !fetch_ok {
        return RemoteHealth::Unreachable;
    }
    match monitor_last_tick.map(|t| (now - t).num_seconds()) {
        Some(age) if age <= STALE_SECS => RemoteHealth::Fresh,
        Some(age) => RemoteHealth::Stale {
            since_secs: age as u64,
        },
        None => RemoteHealth::Stale {
            since_secs: u64::MAX,
        },
    }
}

fn row_key(r: &Row) -> String {
    format!("{}/{}/{}", r.host, r.session, r.segment)
}

impl UpdatesScreen {
    pub fn new() -> Self {
        Self {
            order: Order::MyInput,
            grouping: Grouping::Workspace,
            selected: 0,
            expanded: HashSet::new(),
            rows: vec![],
            remote: RemoteHealth::Fresh,
            files: vec![],
            names: HashMap::new(),
            loaded: false,
        }
    }

    pub fn apply(&mut self, load: UpdatesLoad, names: HashMap<String, String>, now: DateTime<Utc>) {
        self.files = load.files;
        self.names = names;
        self.remote = health(load.last_tick, load.fetch_ok, now);
        self.loaded = true;
        self.rebuild(now);
    }

    /// Rebuild rows, keeping the selection on the same segment.
    fn rebuild(&mut self, now: DateTime<Utc>) {
        let sel = self.rows.get(self.selected).map(row_key);
        self.rows = build_rows(&self.files, self.order, self.grouping, now, &self.names);
        self.selected = sel
            .and_then(|k| self.rows.iter().position(|r| row_key(r) == k))
            .unwrap_or(self.selected)
            .min(self.rows.len().saturating_sub(1));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> UpdatesAction {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('t') => {
                self.order = match self.order {
                    Order::MyInput => Order::AgentResponse,
                    Order::AgentResponse => Order::MyInput,
                };
                self.rebuild(Utc::now());
            }
            KeyCode::Char('g') => {
                self.grouping = match self.grouping {
                    Grouping::Workspace => Grouping::Flat,
                    Grouping::Flat => Grouping::Workspace,
                };
                self.rebuild(Utc::now());
            }
            KeyCode::Char(' ') => {
                if let Some(k) = self.rows.get(self.selected).map(row_key) {
                    if !self.expanded.remove(&k) {
                        self.expanded.insert(k);
                    }
                }
            }
            KeyCode::Enter => {
                let row = self.rows.get(self.selected);
                if let Some(ws) = row.and_then(|r| r.workspace.clone()) {
                    return UpdatesAction::Select {
                        workspace: ws,
                        surface: row.and_then(|r| r.surface.clone()),
                    };
                }
            }
            KeyCode::Char('r') => return UpdatesAction::Refresh,
            KeyCode::Char('u' | 'q') | KeyCode::Esc => return UpdatesAction::Close,
            _ => {}
        }
        UpdatesAction::None
    }
}

/// cmux workspace/surface ids (UUIDs) → the refs `select_workspace` and
/// `focus_surface` take.
pub fn cmux_refs(
    workspaces: &[WorkspaceState],
    workspace: &str,
    surface: Option<&str>,
) -> Option<(String, Option<String>)> {
    let ws = workspaces
        .iter()
        .find(|w| w.workspace.uuid.eq_ignore_ascii_case(workspace))?;
    let surface = surface.and_then(|id| {
        ws.surfaces
            .iter()
            .find(|s| {
                s.uuid
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case(id))
            })
            .map(|s| s.ref_id.clone())
    });
    Some((ws.workspace.ref_id.clone(), surface))
}

/// `(tty, surface uuid, workspace uuid)` for every local surface with a tty.
pub fn surface_ttys(workspaces: &[WorkspaceState]) -> Vec<(String, String, String)> {
    workspaces
        .iter()
        .flat_map(|w| {
            w.surfaces
                .iter()
                .filter_map(|s| Some((s.tty.clone()?, s.uuid.clone()?, w.workspace.uuid.clone())))
        })
        .collect()
}

/// Fetch devbox records (sending mosh hints), store them, then load both hosts.
pub async fn gather(ttys: Vec<(String, String, String)>) -> UpdatesLoad {
    let mut hints = vec![];
    for (tty, surface_id, workspace_id) in ttys {
        let Ok(out) = tokio::process::Command::new("ps")
            .args(["-t", &tty, "-o", "command="])
            .output()
            .await
        else {
            continue;
        };
        if let Some(mosh_port) = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(mosh_port_from_args)
        {
            hints.push(SurfaceHint {
                mosh_port,
                surface_id,
                workspace_id,
            });
        }
    }
    let reply = ArcmuxMeshClient::default()
        .updates(PEER, None, &hints)
        .await
        .ok();
    let fetch_ok = reply.is_some();
    let last_tick = reply
        .as_ref()
        .and_then(|r| r.monitor.as_ref())
        .and_then(|m| DateTime::parse_from_rfc3339(&m.last_tick).ok())
        .map(|t| t.with_timezone(&Utc));
    let files = tokio::task::spawn_blocking(move || {
        if let Some(reply) = &reply {
            let _ = remote::store(PEER, reply);
        }
        let local = updates_dir();
        let mut files = load_dir(&local, "mac");
        files.extend(load_dir(&local.with_file_name(PEER), PEER));
        files
    })
    .await
    .unwrap_or_default();
    UpdatesLoad {
        files,
        last_tick,
        fetch_ok,
    }
}

fn load_dir(dir: &Path, host: &str) -> Vec<SessionFile> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "monitor.json" || name == "hints.json" {
                return None;
            }
            let mut f = record::load(dir, name.strip_suffix(".json")?)?;
            f.host = host.into();
            Some(f)
        })
        .collect()
}

fn banner(h: RemoteHealth) -> Option<String> {
    match h {
        RemoteHealth::Fresh => None,
        RemoteHealth::Stale {
            since_secs: u64::MAX,
        } => Some("devbox monitor: stale (no heartbeat)".into()),
        RemoteHealth::Stale { since_secs } => Some(format!(
            "devbox monitor: stale (last heartbeat {} ago)",
            age(since_secs as i64)
        )),
        RemoteHealth::Unreachable => Some("devbox monitor: unreachable".into()),
    }
}

fn age(secs: i64) -> String {
    match secs {
        ..60 => format!("{}s", secs.max(0)),
        ..3600 => format!("{}m", secs / 60),
        ..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn state_label(s: SegState) -> &'static str {
    match s {
        SegState::Working => "working",
        SegState::WaitingOnMe => "waiting",
        SegState::Blocked => "blocked",
        SegState::Done => "done",
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn render(f: &mut Frame, area: Rect, s: &UpdatesScreen) {
    let order = match s.order {
        Order::MyInput => "by my input",
        Order::AgentResponse => "by agent response",
    };
    let grouping = match s.grouping {
        Grouping::Workspace => "by workspace",
        Grouping::Flat => "flat",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Updates · {order} · {grouping} "));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let banner = banner(s.remote);
    let [top, list] = Layout::vertical([
        Constraint::Length(banner.is_some() as u16),
        Constraint::Min(0),
    ])
    .areas(inner);
    if let Some(b) = banner {
        let style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
        f.render_widget(Paragraph::new(Line::styled(b, style)), top);
    }

    if s.rows.is_empty() {
        let msg = if s.loaded {
            "no updates in the last 7 days"
        } else {
            "loading…"
        };
        f.render_widget(
            Paragraph::new(Line::styled(msg, Style::default().fg(Color::DarkGray))),
            list,
        );
        return;
    }

    let now = Utc::now();
    let mut lines: Vec<Line> = vec![];
    let mut selected_line = 0;
    let mut group: Option<&str> = None;
    for (i, r) in s.rows.iter().enumerate() {
        if group != Some(r.group.as_str()) {
            group = Some(&r.group);
            let style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            lines.push(Line::styled(r.group.clone(), style));
        }
        let mut style = if r.dim {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        if i == s.selected {
            selected_line = lines.len();
            style = style.add_modifier(Modifier::REVERSED);
        }
        let host = if r.host == "mac" {
            String::new()
        } else {
            format!(" [{}]", r.host)
        };
        let latest = if r.latest.is_empty() {
            String::new()
        } else {
            format!("  — {}", one_line(&r.latest))
        };
        lines.push(Line::from(Span::styled(
            format!(
                "  {:>4}  {:<7}  {}{latest}{host}",
                age((now - r.at).num_seconds()),
                state_label(r.state),
                one_line(&r.intent),
            ),
            style,
        )));
        if s.expanded.contains(&row_key(r)) {
            let dim = Style::default().fg(Color::Gray);
            let time = |at: &DateTime<Utc>| at.with_timezone(&Local).format("%m-%d %H:%M");
            for e in &r.entries {
                let text = match e {
                    Entry::Input { at, text, replies } => {
                        format!(
                            "      {}  › {}  ({replies} replies)",
                            time(at),
                            one_line(text)
                        )
                    }
                    Entry::Turn { at, .. } => format!("      {}  ↻ loop turn", time(at)),
                };
                lines.push(Line::styled(text, dim));
            }
            for a in &r.artifacts {
                lines.push(Line::styled(format!("      ⎘ {}", a.reference), dim));
            }
        }
    }
    // Keep the selected row in view.
    let scroll = selected_line.saturating_sub(list.height.saturating_sub(1) as usize);
    f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), list);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mc_data::updates::record::{Segment, UserInput};
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn file(session: &str, ws: Option<&str>, surface: Option<&str>) -> SessionFile {
        let at = Utc::now();
        SessionFile {
            host: "mac".into(),
            session: session.into(),
            agent: "claude".into(),
            transcript: "/t".into(),
            workspace: ws.map(Into::into),
            surface: surface.map(Into::into),
            status: None,
            segments: vec![Segment {
                segment: "s1".into(),
                intent: "ship the updates view".into(),
                latest: "rendering rows".into(),
                latest_at: Some(at),
                state: SegState::Working,
                needs_me: None,
                user_inputs: vec![UserInput {
                    at,
                    text: "build it".into(),
                }],
                agent_turns: vec![],
                artifacts: vec![],
                last_refine_at: None,
            }],
            updated_at: at,
        }
    }

    fn screen(files: Vec<SessionFile>, last_tick: Option<DateTime<Utc>>) -> UpdatesScreen {
        let mut s = UpdatesScreen::new();
        let load = UpdatesLoad {
            files,
            last_tick,
            fetch_ok: true,
        };
        s.apply(load, HashMap::new(), Utc::now());
        s
    }

    #[test]
    fn t_toggles_order() {
        let mut s = screen(vec![file("a", None, None)], Some(Utc::now()));
        assert_eq!(s.handle_key(key(KeyCode::Char('t'))), UpdatesAction::None);
        assert_eq!(s.order, Order::AgentResponse);
        s.handle_key(key(KeyCode::Char('t')));
        assert_eq!(s.order, Order::MyInput);
        assert_eq!(s.rows.len(), 1);
    }

    #[test]
    fn g_toggles_grouping() {
        let mut s = screen(vec![file("a", None, None)], Some(Utc::now()));
        assert_eq!(s.rows[0].group, "mac · unattached");
        s.handle_key(key(KeyCode::Char('g')));
        assert_eq!(s.grouping, Grouping::Flat);
        assert_eq!(s.rows[0].group, "All");
        s.handle_key(key(KeyCode::Char('g')));
        assert_eq!(s.grouping, Grouping::Workspace);
    }

    #[test]
    fn enter_selects_row_surface() {
        let mut s = screen(vec![file("a", Some("WS-1"), Some("SF-1"))], None);
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            UpdatesAction::Select {
                workspace: "WS-1".into(),
                surface: Some("SF-1".into()),
            }
        );
        let mut s = screen(vec![file("a", None, None)], None);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), UpdatesAction::None);
    }

    #[test]
    fn esc_closes() {
        let mut s = UpdatesScreen::new();
        for c in [KeyCode::Esc, KeyCode::Char('u'), KeyCode::Char('q')] {
            assert_eq!(s.handle_key(key(c)), UpdatesAction::Close);
        }
    }

    #[test]
    fn space_toggles_expanded() {
        let mut s = screen(vec![file("a", None, None)], None);
        s.handle_key(key(KeyCode::Char(' ')));
        assert!(s.expanded.contains("mac/a/s1"));
        s.handle_key(key(KeyCode::Char(' ')));
        assert!(s.expanded.is_empty());
    }

    #[test]
    fn health_stale_after_120s() {
        let now = Utc::now();
        let ago = |s| Some(now - chrono::Duration::seconds(s));
        assert_eq!(health(ago(120), true, now), RemoteHealth::Fresh);
        assert_eq!(
            health(ago(121), true, now),
            RemoteHealth::Stale { since_secs: 121 }
        );
        assert_eq!(
            health(None, true, now),
            RemoteHealth::Stale {
                since_secs: u64::MAX
            }
        );
    }

    #[test]
    fn health_unreachable_when_fetch_fails() {
        let now = Utc::now();
        assert_eq!(health(Some(now), false, now), RemoteHealth::Unreachable);
    }

    fn rendered(s: &UpdatesScreen) -> String {
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| render(f, f.area(), s)).unwrap();
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_shows_stale_banner() {
        let tick = Utc::now() - chrono::Duration::seconds(300);
        let s = screen(vec![file("a", None, None)], Some(tick));
        let out = rendered(&s);
        assert!(out.contains("devbox monitor: stale"), "{out}");
        assert!(out.contains("ship the updates view"), "{out}");
        let mut s = UpdatesScreen::new();
        s.remote = RemoteHealth::Unreachable;
        assert!(rendered(&s).contains("devbox monitor: unreachable"));
    }

    #[test]
    fn load_dir_skips_control_files_and_sets_host() {
        let t = tempfile::tempdir().unwrap();
        record::save(t.path(), &file("a", None, None)).unwrap();
        for n in ["monitor.json", "hints.json", ".b.json.tmp"] {
            std::fs::write(t.path().join(n), "{}").unwrap();
        }
        let files = load_dir(t.path(), "devbox");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].host, "devbox");
    }
}
