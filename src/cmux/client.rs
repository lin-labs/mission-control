use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::process::Command;

use crate::mc_data::surface_kind::{self, SurfaceKind};

// ── Transient JSON types for `cmux tree --all --json` ─────────────────────────

#[derive(Deserialize)]
struct TreeJson {
    windows: Vec<TreeWindow>,
}

#[derive(Deserialize)]
struct TreeWindow {
    workspaces: Vec<TreeWorkspace>,
}

#[derive(Deserialize)]
struct TreeWorkspace {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    panes: Vec<TreePane>,
}

#[derive(Deserialize)]
struct TreePane {
    #[serde(default)]
    surfaces: Vec<TreeSurface>,
}

#[derive(Deserialize)]
struct TreeSurface {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    tty: Option<String>,
    /// Zero-based index of this surface within its containing pane.
    #[serde(default)]
    index_in_pane: usize,
}

// ── Transient JSON types for `cmux list-workspaces --json` ────────────────────

#[derive(Deserialize)]
struct WorkspacesJson {
    workspaces: Vec<WorkspaceJson>,
}

#[derive(Deserialize)]
struct WorkspaceJson {
    #[serde(rename = "ref")]
    ref_id: String,
    /// cmux emits the UUID as `id`, and only when `--id-format both` (or `uuids`)
    /// is passed. We always pass `--id-format both` so this is reliable.
    #[serde(rename = "id")]
    uuid: String,
    /// Display name of the workspace (the `title` field in cmux JSON output).
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    selected: bool,
    /// Current working directory of the workspace's active pane, if reported by cmux.
    #[serde(default)]
    current_directory: Option<String>,
    /// User-set workspace color from cmux (`#RRGGBB`), if any. Null when no
    /// color has been assigned via `cmux workspace-action set-color`.
    #[serde(default)]
    custom_color: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub ref_id: String, // e.g. "workspace:2"
    pub uuid: String,   // e.g. "32E47B1E-..."
    pub name: String,   // e.g. "gmail-labs"
    pub selected: bool,
    /// The cmux workspace description (from `cmux workspace-action set-description`).
    /// Non-empty description is used to seed the Goal section of the trajectory doc.
    #[serde(default)]
    pub description: Option<String>,
    /// Current working directory of the workspace's active pane (from cmux JSON).
    /// Used to disambiguate session logs via host+cwd matching.
    #[serde(default)]
    pub current_directory: Option<String>,
    /// User-set workspace color from cmux, as `#RRGGBB`. The sidebar tints the
    /// workspace's row with this color so mc-tui mirrors cmux's visual.
    #[serde(default)]
    pub custom_color: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SurfaceInfo {
    pub title: String,
    /// cmux ref for this surface, e.g. `"surface:92"`.
    pub ref_id: String,
    /// TTY device path, e.g. `"ttys030"`. Useful as a fingerprint for future
    /// per-surface `.session-path` pointer-file injection; unused this iteration.
    pub tty: Option<String>,
    /// Zero-based index of this surface within its pane (from `index_in_pane`).
    pub index_in_pane: usize,
    /// Detected kind of the foreground process on this surface's tty
    /// (Claude / Codex / Shell / …). `Unknown` when `tty` is `None` or
    /// detection failed. Populated by `surface_kind::detect`.
    pub kind: SurfaceKind,
}

#[derive(Clone)]
pub struct CmuxClient {
    bin: String,
    socket_path: PathBuf,
}

impl CmuxClient {
    pub fn new(bin: String, socket_path: PathBuf) -> Self {
        Self { bin, socket_path }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.env("CMUX_SOCKET_PATH", &self.socket_path);
        cmd
    }

    /// Parse `cmux list-workspaces --json --id-format both` output.
    /// JSON shape: `{ "workspaces": [{ "ref": "workspace:N", "id": "<uuid>",
    ///               "title": "name", "description": null, "selected": false, ... }] }`
    ///
    /// `--id-format both` is required — without it the JSON omits the `id`
    /// (UUID) field entirely, which we need to key per-workspace data dirs.
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let output = self
            .cmd()
            .args(["list-workspaces", "--json", "--id-format", "both"])
            .output()
            .await
            .context("failed to run cmux list-workspaces --json --id-format both")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("cmux list-workspaces failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: WorkspacesJson = serde_json::from_str(&stdout)
            .context("failed to parse cmux list-workspaces --json output")?;

        let workspaces = parsed
            .workspaces
            .into_iter()
            .map(|w| Workspace {
                ref_id: w.ref_id,
                uuid: w.uuid,
                name: w.title,
                selected: w.selected,
                description: w.description,
                current_directory: w.current_directory,
                custom_color: w.custom_color,
            })
            .collect();

        Ok(workspaces)
    }

    /// Set (or clear) the cmux workspace description for the given workspace ref.
    ///
    /// This is used to push the Goal section of the trajectory back to cmux so
    /// that the description is visible in the workspace tab tooltip.
    ///
    /// Non-fatal by convention: callers should log errors to stderr and continue.
    pub async fn set_workspace_description(
        &self,
        workspace_ref: &str,
        description: &str,
    ) -> Result<()> {
        let action = if description.is_empty() {
            "clear-description"
        } else {
            "set-description"
        };
        let mut cmd = self.cmd();
        cmd.arg("workspace-action")
            .arg("--workspace")
            .arg(workspace_ref)
            .arg("--action")
            .arg(action);
        if !description.is_empty() {
            cmd.arg("--description").arg(description);
        }
        let output = cmd
            .output()
            .await
            .context("run cmux workspace-action set-description")?;
        if !output.status.success() {
            anyhow::bail!(
                "cmux workspace-action {} failed: {}",
                action,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    /// Read the last N lines of a *workspace's* current screen via
    /// `cmux read-screen --workspace`. Collapses N surfaces onto one
    /// stream — for per-surface reads, prefer `read_surface_text`.
    /// Kept for compatibility with callers that genuinely want the
    /// workspace-level view.
    pub async fn read_screen(&self, workspace_ref: &str, lines: u32) -> Result<String> {
        let output = self
            .cmd()
            .args([
                "read-screen",
                "--workspace",
                workspace_ref,
                "--lines",
                &lines.to_string(),
            ])
            .output()
            .await
            .context("failed to run cmux read-screen")?;

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Read the last N lines of a *specific surface's* screen via
    /// `cmux rpc surface.read_text`. This is the per-surface analog of
    /// `read_screen` — peek mode uses this so non-agent surfaces in the
    /// same workspace don't all show the same content (F11 in
    /// `.agents/validate.md`).
    ///
    /// `surface_ref` accepts a surface ref (e.g. `"surface:121"`) or a
    /// surface UUID; cmux is flexible on the value.
    pub async fn read_surface_text(&self, surface_ref: &str, lines: u32) -> Result<String> {
        // Build the JSON params: {"surface_id": <ref>, "lines": N}. Escape
        // is unnecessary because surface refs are `surface:<digits>`.
        let params = format!(
            r#"{{"surface_id":"{}","lines":{}}}"#,
            surface_ref, lines
        );
        let output = self
            .cmd()
            .args(["rpc", "surface.read_text", &params])
            .output()
            .await
            .context("failed to run cmux rpc surface.read_text")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("cmux rpc surface.read_text failed: {}", stderr);
        }

        // The response is a JSON object with a `text` field. Use
        // serde_json so we don't have to escape-parse the inner content.
        #[derive(serde::Deserialize)]
        struct Response {
            text: String,
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: Response = serde_json::from_str(&stdout)
            .with_context(|| {
                format!(
                    "failed to parse cmux rpc surface.read_text output for {}",
                    surface_ref
                )
            })?;
        Ok(parsed.text)
    }

    /// Select a workspace (focus it).
    pub async fn select_workspace(&self, workspace_ref: &str) -> Result<()> {
        self.cmd()
            .args(["select-workspace", "--workspace", workspace_ref])
            .output()
            .await
            .context("failed to run cmux select-workspace")?;
        Ok(())
    }

    /// Parse `cmux tree --all --json` to get structured per-workspace surface lists.
    ///
    /// Returns a map from workspace ref (e.g. `"workspace:25"`) to the ordered
    /// list of surfaces in that workspace's panes. Each `SurfaceInfo` carries
    /// the surface's own ref_id (e.g. `"surface:92"`), tty, and index_in_pane
    /// so peek mode can distinguish surfaces within the same workspace.
    pub async fn get_surfaces_json(&self) -> Result<HashMap<String, Vec<SurfaceInfo>>> {
        let output = self.cmd()
            .args(["tree", "--all", "--json"])
            .output()
            .await
            .context("failed to run cmux tree --all --json")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("cmux tree --all --json failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: TreeJson = serde_json::from_str(&stdout)
            .context("failed to parse cmux tree --all --json output")?;

        // First pass: collect every non-empty tty across all surfaces. Then
        // resolve all of them in a SINGLE `ps -A` call via `detect_all`. This
        // turns ~60 subprocess spawns per refresh (lsof + ps per surface) into
        // one — the difference shows up as roughly 1.5%→~0.3% steady-state CPU
        // on a sidebar with ~30 surfaces.
        let mut ttys: Vec<&str> = Vec::new();
        for window in &parsed.windows {
            for ws in &window.workspaces {
                for pane in &ws.panes {
                    for s in &pane.surfaces {
                        if let Some(tty) = s.tty.as_deref() {
                            if !tty.is_empty() {
                                ttys.push(tty);
                            }
                        }
                    }
                }
            }
        }
        let kinds_by_tty = surface_kind::detect_all(&ttys);

        let mut result: HashMap<String, Vec<SurfaceInfo>> = HashMap::new();
        for window in parsed.windows {
            for ws in window.workspaces {
                let surfaces: Vec<SurfaceInfo> = ws
                    .panes
                    .into_iter()
                    .flat_map(|pane| pane.surfaces)
                    .map(|s| {
                        let kind = match s.tty.as_deref() {
                            Some(tty) if !tty.is_empty() => kinds_by_tty
                                .get(tty)
                                .copied()
                                .unwrap_or(SurfaceKind::Unknown),
                            _ => SurfaceKind::Unknown,
                        };
                        SurfaceInfo {
                            title: s.title,
                            ref_id: s.ref_id,
                            tty: s.tty,
                            index_in_pane: s.index_in_pane,
                            kind,
                        }
                    })
                    .collect();
                if !surfaces.is_empty() {
                    result.insert(ws.ref_id, surfaces);
                }
            }
        }

        Ok(result)
    }

    /// Parse `cmux tree --all` (text) to get surface titles per workspace ref.
    ///
    /// Kept for backwards-compatibility. New callers should use `get_surfaces_json`
    /// which provides per-surface ref_id and tty in addition to title.
    pub async fn get_surfaces(&self) -> Result<HashMap<String, Vec<SurfaceInfo>>> {
        // Delegate to the JSON parser which is strictly more accurate.
        self.get_surfaces_json().await
    }

    /// Send raw text to a cmux surface (terminal only).
    ///
    /// Wraps `cmux send --workspace <ws> --surface <s> <text>`. The `--workspace`
    /// flag is mandatory — without it cmux errors out with "Surface is not a
    /// terminal" regardless of the surface's actual kind. (Verified live on
    /// 2026-05-24 against cmux 0.x.)
    ///
    /// On success cmux prints `OK <surface_ref> <workspace_ref>` to stdout and
    /// exits 0. On failure (bad ref, non-terminal surface, etc.) it prints an
    /// `Error: <kind>: <message>` line to stderr and exits non-zero.
    pub async fn send_text(
        &self,
        workspace_ref: &str,
        surface_ref: &str,
        text: &str,
    ) -> Result<()> {
        let output = self
            .cmd()
            .args([
                "send",
                "--workspace",
                workspace_ref,
                "--surface",
                surface_ref,
                text,
            ])
            .output()
            .await
            .context("failed to run cmux send")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("cmux send failed: {}", stderr);
        }
        Ok(())
    }

    /// Create a new surface in the given workspace.
    ///
    /// Wraps `cmux new-surface --type <surface_type> --workspace <ws>`. On
    /// success cmux prints a single line of the form:
    ///   `OK surface:<N> pane:<M> workspace:<K>`
    /// and exits 0. We parse out the `surface:<N>` token (the new surface ref)
    /// and return it. (Verified live on 2026-05-24.)
    ///
    /// On failure cmux prints `Error: <kind>: <message>` on stderr.
    pub async fn new_surface(
        &self,
        workspace_ref: &str,
        surface_type: &str,
    ) -> Result<String> {
        let output = self
            .cmd()
            .args([
                "new-surface",
                "--type",
                surface_type,
                "--workspace",
                workspace_ref,
            ])
            .output()
            .await
            .context("failed to run cmux new-surface")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("cmux new-surface failed: {}", stderr);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // First whitespace-separated token starting with "surface:" is the ref.
        for token in stdout.split_whitespace() {
            if let Some(stripped) = token.strip_prefix("surface:") {
                if !stripped.is_empty() {
                    return Ok(token.to_string());
                }
            }
        }
        anyhow::bail!(
            "cmux new-surface succeeded but did not emit a surface:<N> ref; stdout was: {}",
            stdout.trim()
        );
    }
}

