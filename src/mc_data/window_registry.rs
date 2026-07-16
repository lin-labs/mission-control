use crate::cmux::client::{SurfaceInfo, Workspace};
use crate::mc_data::session_log::{ConversationIntent, Frontmatter};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct SurfaceSessionRecord {
    pub path: PathBuf,
    pub frontmatter: Frontmatter,
    pub intent: ConversationIntent,
}

#[derive(Debug, Clone)]
pub struct RegistryBuildOutput {
    pub registry: WindowRegistry,
    pub repo_roots_by_ws_id: HashMap<String, Vec<PathBuf>>,
    pub repo_by_surface_by_ws_id: HashMap<String, HashMap<String, PathBuf>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRegistry {
    pub schema_version: u32,
    pub window_id: String,
    pub window_ref: Option<String>,
    pub updated_at: String,
    pub histories_dir: PathBuf,
    pub histories_valid: bool,
    pub workspaces: Vec<WorkspaceRegistration>,
    pub surfaces: Vec<SurfaceRegistration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRegistration {
    pub schema_version: u32,
    pub window_id: String,
    pub window_ref: Option<String>,
    pub workspace_id: String,
    pub workspace_ref: String,
    pub name: String,
    pub current_directory: Option<PathBuf>,
    pub repo_roots: Vec<PathBuf>,
    pub surface_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceRegistration {
    pub schema_version: u32,
    pub window_id: String,
    pub window_ref: Option<String>,
    pub workspace_id: String,
    pub workspace_ref: String,
    pub surface_ref: String,
    pub pane_ref: Option<String>,
    pub title: String,
    pub tty: Option<String>,
    pub kind: crate::mc_data::surface_kind::SurfaceKind,
    pub selected: bool,
    pub focused: bool,
    pub active: bool,
    pub index: Option<u32>,
    pub index_in_pane: Option<u32>,
    pub surface_type: Option<String>,
    pub repo_root: Option<PathBuf>,
    pub repo_source: Option<String>,
    pub session_path: Option<PathBuf>,
    pub session_cwd: Option<PathBuf>,
    pub overall_goal: Option<String>,
    pub latest_ask: Option<String>,
}

pub fn build_registry(
    workspaces: &[Workspace],
    surfaces_map: &HashMap<String, Vec<SurfaceInfo>>,
    surface_sessions_by_ws_id: &HashMap<String, HashMap<String, SurfaceSessionRecord>>,
    histories_dir: &Path,
    histories_valid: bool,
) -> RegistryBuildOutput {
    let window_id = workspaces
        .iter()
        .find_map(|ws| ws.window_id.clone())
        .unwrap_or_else(|| "unknown-window".to_string());
    let window_ref = workspaces.iter().find_map(|ws| ws.window_ref.clone());
    let updated_at = chrono::Utc::now().to_rfc3339();

    let mut workspace_regs = Vec::new();
    let mut surface_regs = Vec::new();
    let mut repo_roots_by_ws_id = HashMap::new();
    let mut repo_by_surface_by_ws_id = HashMap::new();

    // cmux's authoritative per-surface agent binding (surface UUID -> bound
    // session cwd/transcript). The highest-priority repo source: it reflects
    // where the agent actually is, not a fuzzy session-log match.
    let bound_by_surface = crate::mc_data::cmux_sessions::load_by_surface();

    for ws in workspaces {
        let workspace_repo = ws
            .current_directory
            .as_deref()
            .and_then(path_from_str)
            .and_then(|path| git_root_for_path(&path));
        let surfaces = surfaces_map.get(&ws.ref_id).cloned().unwrap_or_default();
        let session_records = surface_sessions_by_ws_id.get(&ws.uuid);
        let mut ordered_repo_roots = Vec::new();
        let mut seen_repos = HashSet::new();
        let mut surface_refs = Vec::new();
        let mut repo_by_surface = HashMap::new();

        for surface in &surfaces {
            surface_refs.push(surface.ref_id.clone());
            let session_record = session_records.and_then(|records| records.get(&surface.ref_id));
            let bound = surface
                .uuid
                .as_deref()
                .and_then(|id| bound_by_surface.get(id));
            let (repo_root, repo_source) = infer_surface_repo(
                surface,
                session_record,
                workspace_repo.as_ref(),
                bound.and_then(|b| b.cwd.as_deref()),
            );
            if let Some(repo) = repo_root.as_ref() {
                repo_by_surface.insert(surface.ref_id.clone(), repo.clone());
                if seen_repos.insert(repo.clone()) {
                    ordered_repo_roots.push(repo.clone());
                }
            }
            surface_regs.push(SurfaceRegistration {
                schema_version: SCHEMA_VERSION,
                window_id: window_id.clone(),
                window_ref: window_ref.clone(),
                workspace_id: ws.uuid.clone(),
                workspace_ref: ws.ref_id.clone(),
                surface_ref: surface.ref_id.clone(),
                pane_ref: surface.pane_ref.clone(),
                title: surface.title.clone(),
                tty: surface.tty.clone(),
                kind: surface.kind,
                selected: surface.selected,
                focused: surface.focused,
                active: surface.active,
                index: surface.index,
                index_in_pane: surface.index_in_pane,
                surface_type: surface.surface_type.clone(),
                repo_root,
                repo_source,
                session_path: session_record.map(|record| record.path.clone()),
                session_cwd: session_record
                    .and_then(|record| record.frontmatter.cwd.as_deref())
                    .and_then(path_from_str),
                overall_goal: session_record.and_then(|record| record.intent.overall_goal.clone()),
                latest_ask: session_record.and_then(|record| record.intent.latest_ask.clone()),
            });
        }

        if let Some(repo) = workspace_repo {
            if seen_repos.insert(repo.clone()) {
                ordered_repo_roots.push(repo);
            }
        }

        repo_roots_by_ws_id.insert(ws.uuid.clone(), ordered_repo_roots.clone());
        repo_by_surface_by_ws_id.insert(ws.uuid.clone(), repo_by_surface);
        workspace_regs.push(WorkspaceRegistration {
            schema_version: SCHEMA_VERSION,
            window_id: window_id.clone(),
            window_ref: window_ref.clone(),
            workspace_id: ws.uuid.clone(),
            workspace_ref: ws.ref_id.clone(),
            name: ws.name.clone(),
            current_directory: ws.current_directory.as_deref().and_then(path_from_str),
            repo_roots: ordered_repo_roots,
            surface_refs,
        });
    }

    RegistryBuildOutput {
        registry: WindowRegistry {
            schema_version: SCHEMA_VERSION,
            window_id,
            window_ref,
            updated_at,
            histories_dir: histories_dir.to_path_buf(),
            histories_valid,
            workspaces: workspace_regs,
            surfaces: surface_regs,
        },
        repo_roots_by_ws_id,
        repo_by_surface_by_ws_id,
    }
}

pub fn write_registry(registry: &WindowRegistry) -> Result<()> {
    let dir = crate::mc_data::paths::window_dir(&registry.window_id);
    let workspaces_dir = dir.join("workspaces");
    let surfaces_dir = dir.join("surfaces");
    std::fs::create_dir_all(&workspaces_dir)
        .with_context(|| format!("create {}", workspaces_dir.display()))?;
    std::fs::create_dir_all(&surfaces_dir)
        .with_context(|| format!("create {}", surfaces_dir.display()))?;

    write_json(&dir.join("window.json"), registry)?;

    let mut workspace_files = HashSet::new();
    for workspace in &registry.workspaces {
        let file = format!(
            "{}.json",
            crate::mc_data::paths::safe_path_component(&workspace.workspace_id)
        );
        workspace_files.insert(file.clone());
        write_json(&workspaces_dir.join(file), workspace)?;
    }
    prune_json_dir(&workspaces_dir, &workspace_files)?;

    let mut surface_files = HashSet::new();
    for surface in &registry.surfaces {
        let file = format!(
            "{}.json",
            crate::mc_data::paths::safe_path_component(&surface.surface_ref)
        );
        surface_files.insert(file.clone());
        write_json(&surfaces_dir.join(file), surface)?;
    }
    prune_json_dir(&surfaces_dir, &surface_files)?;

    Ok(())
}

fn infer_surface_repo(
    surface: &SurfaceInfo,
    session_record: Option<&SurfaceSessionRecord>,
    workspace_repo: Option<&PathBuf>,
    bound_cwd: Option<&Path>,
) -> (Option<PathBuf>, Option<String>) {
    // Highest priority: cmux's per-surface binding (where the agent actually
    // is). This authoritatively scopes the surface's repo and stops a stale
    // session-log cwd or a sibling surface from leaking another project in.
    if let Some(repo) = bound_cwd.and_then(git_root_for_path) {
        return (Some(repo), Some("cmux_bind".to_string()));
    }
    if let Some(repo) = session_record
        .and_then(|record| record.frontmatter.cwd.as_deref())
        .and_then(path_from_str)
        .and_then(|path| git_root_for_path(&path))
    {
        return (Some(repo), Some("session_cwd".to_string()));
    }
    if let Some(repo) =
        path_from_surface_title(&surface.title).and_then(|path| git_root_for_path(&path))
    {
        return (Some(repo), Some("surface_title".to_string()));
    }
    if let Some(repo) = workspace_repo {
        return (Some(repo.clone()), Some("workspace_cwd".to_string()));
    }
    (None, None)
}

pub fn git_root_for_path(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["-C", path.to_str()?, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn path_from_surface_title(title: &str) -> Option<PathBuf> {
    title.split_whitespace().find_map(|token| {
        let token = token.trim_matches(|ch: char| {
            matches!(ch, '"' | '\'' | '`' | '(' | ')' | '[' | ']' | ',' | ';')
        });
        if token.starts_with("~/") || token.starts_with('/') {
            path_from_str(token)
        } else if token.contains(':') {
            token
                .rsplit_once(':')
                .map(|(_, path)| path)
                .filter(|path| path.starts_with("~/") || path.starts_with('/'))
                .and_then(path_from_str)
        } else {
            None
        }
    })
}

fn path_from_str(path: &str) -> Option<PathBuf> {
    if path.trim().is_empty() {
        return None;
    }
    if path == "~" {
        return dirs::home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return dirs::home_dir().map(|home| home.join(rest));
    }
    Some(PathBuf::from(path))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn prune_json_dir(dir: &Path, keep: &HashSet<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !keep.contains(name) {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mc_data::surface_kind::SurfaceKind;

    struct HomeGuard(Option<std::ffi::OsString>);
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    fn init_git_repo(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed in {}", root.display());
    }

    fn workspace(cwd: &Path) -> Workspace {
        Workspace {
            window_id: Some("window-abc/123".to_string()),
            window_ref: Some("window:7".to_string()),
            ref_id: "workspace:1".to_string(),
            uuid: "ws-1".to_string(),
            name: "multi-repo".to_string(),
            description: None,
            current_directory: Some(cwd.to_string_lossy().to_string()),
            custom_color: None,
        }
    }

    fn surface(ref_id: &str, title: String) -> SurfaceInfo {
        SurfaceInfo {
            title,
            ref_id: ref_id.to_string(),
            uuid: None,
            pane_ref: Some("pane:1".to_string()),
            tty: Some("ttys001".to_string()),
            kind: SurfaceKind::Claude,
            selected: false,
            focused: false,
            active: false,
            index: Some(0),
            index_in_pane: Some(0),
            surface_type: Some("terminal".to_string()),
        }
    }

    #[test]
    fn registry_orders_repos_from_surface_context_before_workspace_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");
        init_git_repo(&repo_a);
        init_git_repo(&repo_b);
        let repo_a = std::fs::canonicalize(repo_a).unwrap();
        let repo_b = std::fs::canonicalize(repo_b).unwrap();

        let workspaces = vec![workspace(&repo_a)];
        let surfaces = vec![
            surface("surface:1", "claude".to_string()),
            surface("surface:2", format!("blin@mbp:{}", repo_a.display())),
        ];
        let mut surfaces_map = HashMap::new();
        surfaces_map.insert("workspace:1".to_string(), surfaces);

        let mut surface_sessions = HashMap::new();
        surface_sessions.insert(
            "surface:1".to_string(),
            SurfaceSessionRecord {
                path: tmp.path().join("session.md"),
                frontmatter: Frontmatter {
                    workspace_id: Some("ws-1".to_string()),
                    host: Some("mbp".to_string()),
                    cwd: Some(repo_b.to_string_lossy().to_string()),
                    agent: Some("claude".to_string()),
                },
                intent: ConversationIntent {
                    overall_goal: Some("ship registry".to_string()),
                    latest_ask: Some("wire multi repo beads".to_string()),
                },
            },
        );
        let mut surface_sessions_by_ws_id = HashMap::new();
        surface_sessions_by_ws_id.insert("ws-1".to_string(), surface_sessions);

        let output = build_registry(
            &workspaces,
            &surfaces_map,
            &surface_sessions_by_ws_id,
            &tmp.path().join("histories"),
            true,
        );

        assert_eq!(
            output.repo_roots_by_ws_id.get("ws-1").unwrap(),
            &vec![repo_b.clone(), repo_a.clone()]
        );
        assert_eq!(
            output
                .repo_by_surface_by_ws_id
                .get("ws-1")
                .unwrap()
                .get("surface:1"),
            Some(&repo_b)
        );
        assert_eq!(
            output
                .repo_by_surface_by_ws_id
                .get("ws-1")
                .unwrap()
                .get("surface:2"),
            Some(&repo_a)
        );
        assert_eq!(output.registry.window_id, "window-abc/123");
        assert_eq!(
            output.registry.workspaces[0].repo_roots,
            vec![repo_b, repo_a]
        );
        assert_eq!(
            output.registry.surfaces[0].latest_ask.as_deref(),
            Some("wire multi repo beads")
        );
    }

    #[test]
    fn write_registry_creates_human_readable_window_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = HomeGuard(std::env::var_os("HOME"));
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let registry = WindowRegistry {
            schema_version: SCHEMA_VERSION,
            window_id: "window:7".to_string(),
            window_ref: Some("window:7".to_string()),
            updated_at: "2026-06-04T00:00:00Z".to_string(),
            histories_dir: tmp.path().join("agents/histories"),
            histories_valid: true,
            workspaces: vec![WorkspaceRegistration {
                schema_version: SCHEMA_VERSION,
                window_id: "window:7".to_string(),
                window_ref: Some("window:7".to_string()),
                workspace_id: "ws-1".to_string(),
                workspace_ref: "workspace:1".to_string(),
                name: "repo".to_string(),
                current_directory: Some(tmp.path().join("repo")),
                repo_roots: vec![tmp.path().join("repo")],
                surface_refs: vec!["surface:1".to_string()],
            }],
            surfaces: vec![SurfaceRegistration {
                schema_version: SCHEMA_VERSION,
                window_id: "window:7".to_string(),
                window_ref: Some("window:7".to_string()),
                workspace_id: "ws-1".to_string(),
                workspace_ref: "workspace:1".to_string(),
                surface_ref: "surface:1".to_string(),
                pane_ref: Some("pane:1".to_string()),
                title: "claude".to_string(),
                tty: Some("ttys001".to_string()),
                kind: SurfaceKind::Claude,
                selected: true,
                focused: true,
                active: true,
                index: Some(0),
                index_in_pane: Some(0),
                surface_type: Some("terminal".to_string()),
                repo_root: Some(tmp.path().join("repo")),
                repo_source: Some("session_cwd".to_string()),
                session_path: Some(tmp.path().join("session.md")),
                session_cwd: Some(tmp.path().join("repo")),
                overall_goal: Some("ship".to_string()),
                latest_ask: Some("write files".to_string()),
            }],
        };

        write_registry(&registry).unwrap();

        let base = tmp.path().join("data/mission-control/windows/window_7");
        assert!(base.join("window.json").is_file());
        assert!(base.join("workspaces/ws-1.json").is_file());
        assert!(base.join("surfaces/surface_1.json").is_file());

        let window = std::fs::read_to_string(base.join("window.json")).unwrap();
        assert!(window.contains("\"histories_valid\": true"));
        assert!(window.contains("\"workspaces\""));
        assert!(window.contains("\"surfaces\""));
    }
}
