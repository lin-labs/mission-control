use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::mc_data::surface_kind::{self, SurfaceKind};

const TRANSACTION_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const TRANSACTION_READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const TRANSACTION_MAX_OUTPUT_BYTES: usize = 256 * 1024;

// ── Transient JSON types for `cmux tree --all --json` ─────────────────────────

#[derive(Deserialize)]
struct TreeJson {
    windows: Vec<TreeWindow>,
}

#[derive(Deserialize)]
struct TreeWindow {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    current: bool,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    key: bool,
    workspaces: Vec<TreeWorkspace>,
}

#[derive(Deserialize)]
struct TreeWorkspace {
    #[serde(rename = "ref")]
    ref_id: String,
    /// cmux workspace UUID (with `--id-format both`). Used to collect the full
    /// live workspace set for active/archived lifecycle decisions.
    #[serde(default, rename = "id")]
    uuid: Option<String>,
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
    /// cmux surface UUID (the `id` field, present with `--id-format both`).
    /// Used to key into the cmux hook-session binding registry.
    #[serde(default, rename = "id")]
    uuid: Option<String>,
    #[serde(default)]
    pane_ref: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    tty: Option<String>,
    #[serde(default)]
    selected: bool,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    index_in_pane: Option<u32>,
    #[serde(default, rename = "type")]
    surface_type: Option<String>,
}

// ── Transient JSON types for `cmux list-workspaces --json` ────────────────────

#[derive(Deserialize)]
struct WorkspacesJson {
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    window_ref: Option<String>,
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
    /// cmux window UUID containing this workspace.
    #[serde(default)]
    pub window_id: Option<String>,
    /// cmux window ref containing this workspace, e.g. `"window:1"`.
    #[serde(default)]
    pub window_ref: Option<String>,
    pub name: String, // e.g. "gmail-labs"
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
    /// cmux surface UUID (`id`), used to key into the hook-session binding
    /// registry (`cmux_sessions`). `None` if cmux didn't report it.
    pub uuid: Option<String>,
    /// cmux pane ref for this surface, e.g. `"pane:6"`.
    pub pane_ref: Option<String>,
    /// TTY device path, e.g. `"ttys030"`. Useful as a fingerprint for future
    /// per-surface `.session-path` pointer-file injection; unused this iteration.
    pub tty: Option<String>,
    /// Detected kind of the foreground process on this surface's tty
    /// (Claude / Codex / Shell / …). `Unknown` when `tty` is `None` or
    /// detection failed. Populated by `surface_kind::detect`.
    pub kind: SurfaceKind,
    pub selected: bool,
    pub focused: bool,
    pub active: bool,
    pub index: Option<u32>,
    pub index_in_pane: Option<u32>,
    pub surface_type: Option<String>,
}

#[derive(Clone)]
pub struct CmuxClient {
    bin: String,
    socket_path: PathBuf,
    transaction_command_timeout: Duration,
    transaction_reader_drain_timeout: Duration,
    transaction_max_output_bytes: usize,
}

struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CmuxClient {
    pub fn new(bin: String, socket_path: PathBuf) -> Self {
        Self {
            bin,
            socket_path,
            transaction_command_timeout: TRANSACTION_COMMAND_TIMEOUT,
            transaction_reader_drain_timeout: TRANSACTION_READER_DRAIN_TIMEOUT,
            transaction_max_output_bytes: TRANSACTION_MAX_OUTPUT_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_transaction_limits(
        bin: String,
        socket_path: PathBuf,
        command_timeout: Duration,
        reader_drain_timeout: Duration,
        max_output_bytes: usize,
    ) -> Self {
        Self {
            bin,
            socket_path,
            transaction_command_timeout: command_timeout,
            transaction_reader_drain_timeout: reader_drain_timeout,
            transaction_max_output_bytes: max_output_bytes,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.env("CMUX_SOCKET_PATH", &self.socket_path);
        cmd
    }

    /// Run one identity-sensitive cmux transaction command with bounded time,
    /// retained output, and pipe-drain lifetime. A grandchild retaining the
    /// inherited stdout/stderr descriptors must not hold Mission Control open
    /// after the direct cmux child has exited.
    async fn bounded_transaction_output(&self, args: &[&str]) -> Result<BoundedCommandOutput> {
        let mut command = self.cmd();
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("failed to start bounded cmux command")?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("bounded cmux stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("bounded cmux stderr unavailable"))?;
        let mut stdout_task = BoundedReadTask::new(tokio::spawn(read_bounded_output(
            stdout,
            self.transaction_max_output_bytes,
        )));
        let mut stderr_task = BoundedReadTask::new(tokio::spawn(read_bounded_output(
            stderr,
            self.transaction_max_output_bytes,
        )));

        let status = match timeout(self.transaction_command_timeout, child.wait()).await {
            Ok(result) => result.context("bounded cmux command wait failed")?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = collect_bounded_output(
                    &mut stdout_task,
                    &mut stderr_task,
                    self.transaction_reader_drain_timeout,
                )
                .await;
                anyhow::bail!("cmux command timed out");
            }
        };
        let ((stdout, stdout_overflow), (stderr, stderr_overflow)) = collect_bounded_output(
            &mut stdout_task,
            &mut stderr_task,
            self.transaction_reader_drain_timeout,
        )
        .await?;
        if stdout_overflow || stderr_overflow {
            anyhow::bail!("cmux command output exceeded the safety limit");
        }
        Ok(BoundedCommandOutput {
            status,
            stdout,
            stderr,
        })
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
        let window_id = parsed.window_id.clone();
        let window_ref = parsed.window_ref.clone();

        let workspaces = parsed
            .workspaces
            .into_iter()
            .map(|w| Workspace {
                ref_id: w.ref_id,
                uuid: w.uuid,
                window_id: window_id.clone(),
                window_ref: window_ref.clone(),
                name: w.title,
                description: w.description,
                current_directory: w.current_directory,
                custom_color: w.custom_color,
            })
            .collect();

        Ok(workspaces)
    }

    /// Full set of live workspace UUIDs across ALL cmux windows (via
    /// `tree --all`). Used to decide which `active/` workspace dirs are now
    /// closed and should be archived. Errors / empty are surfaced as-is so the
    /// caller can refuse to archive on an unreliable (empty) result.
    pub async fn all_workspace_uuids(&self) -> Result<std::collections::HashSet<String>> {
        let out = self
            .cmd()
            .args(["tree", "--all", "--json", "--id-format", "both"])
            .output()
            .await
            .context("failed to run cmux tree --all for workspace uuids")?;
        if !out.status.success() {
            anyhow::bail!("cmux tree --all failed");
        }
        let tree: TreeJson = serde_json::from_slice(&out.stdout)
            .context("failed to parse cmux tree --all for workspace uuids")?;
        let mut set = std::collections::HashSet::new();
        for w in tree.windows {
            for ws in w.workspaces {
                if let Some(id) = ws.uuid {
                    set.insert(id);
                }
            }
        }
        Ok(set)
    }

    /// Focus a specific surface (by ref or UUID) — reveals its window /
    /// workspace / **pane** / tab. Call after `select_workspace` so "go to
    /// surface" lands on the right split pane, not the workspace's last-focused
    /// one. Uses `surface.focus`, which resolves the surface's pane itself.
    pub async fn focus_surface(&self, surface_ref: &str) -> Result<()> {
        let params = format!(r#"{{"surface_id":"{surface_ref}"}}"#);
        let output = self
            .cmd()
            .args(["rpc", "surface.focus", &params])
            .output()
            .await
            .context("failed to run cmux rpc surface.focus")?;
        if !output.status.success() {
            anyhow::bail!(
                "cmux rpc surface.focus failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
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
        let params = format!(r#"{{"surface_id":"{}","lines":{}}}"#, surface_ref, lines);
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
        let parsed: Response = serde_json::from_str(&stdout).with_context(|| {
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
    /// the surface's own ref_id (e.g. `"surface:92"`) and tty so peek mode can
    /// distinguish surfaces within the same workspace.
    #[allow(dead_code)]
    pub async fn get_surfaces_json(&self) -> Result<HashMap<String, Vec<SurfaceInfo>>> {
        self.get_surfaces_json_for_window(None).await
    }

    pub async fn get_surfaces_json_for_window(
        &self,
        expected_window_ref: Option<&str>,
    ) -> Result<HashMap<String, Vec<SurfaceInfo>>> {
        let output = self
            .cmd()
            .args(["tree", "--all", "--json", "--id-format", "both"])
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
        let current_window_ref = current_window_ref(&parsed, expected_window_ref)?;
        for window in parsed
            .windows
            .iter()
            .filter(|window| Some(window.ref_id.as_str()) == current_window_ref.as_deref())
        {
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
        for window in parsed
            .windows
            .into_iter()
            .filter(|window| Some(window.ref_id.as_str()) == current_window_ref.as_deref())
        {
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
                            uuid: s.uuid,
                            pane_ref: s.pane_ref,
                            tty: s.tty,
                            kind,
                            selected: s.selected,
                            focused: s.focused,
                            active: s.active,
                            index: s.index,
                            index_in_pane: s.index_in_pane,
                            surface_type: s.surface_type,
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
    #[allow(dead_code)]
    pub async fn get_surfaces(&self) -> Result<HashMap<String, Vec<SurfaceInfo>>> {
        // Delegate to the JSON parser which is strictly more accurate.
        self.get_surfaces_json().await
    }

    pub async fn get_surfaces_for_window(
        &self,
        expected_window_ref: Option<&str>,
    ) -> Result<HashMap<String, Vec<SurfaceInfo>>> {
        self.get_surfaces_json_for_window(expected_window_ref).await
    }

    /// Resolve one newly-created cmux `surface:N` ref to its stable UUID.
    ///
    /// The join is deliberately exact across the window ref, workspace ref,
    /// and surface ref. Dispatch must never recover a UUID by matching a title,
    /// cwd, focus state, or newest surface (F1/F10/F16).
    pub async fn exact_surface_uuid(
        &self,
        expected_window_ref: &str,
        workspace_ref: &str,
        surface_ref: &str,
    ) -> Result<String> {
        let output = self
            .bounded_transaction_output(&["tree", "--all", "--json", "--id-format", "both"])
            .await
            .context("failed to run cmux tree for exact surface UUID")?;
        if !output.status.success() {
            anyhow::bail!(
                "cmux tree failed while resolving exact surface UUID: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let tree: TreeJson = serde_json::from_slice(&output.stdout)
            .context("failed to parse cmux tree for exact surface UUID")?;
        let mut matches = tree
            .windows
            .into_iter()
            .filter(|window| window.ref_id == expected_window_ref)
            .flat_map(|window| window.workspaces)
            .filter(|workspace| workspace.ref_id == workspace_ref)
            .flat_map(|workspace| workspace.panes)
            .flat_map(|pane| pane.surfaces)
            .filter(|surface| surface.ref_id == surface_ref);
        let surface = matches
            .next()
            .ok_or_else(|| anyhow::anyhow!("exact cmux surface is not present in tree"))?;
        if matches.next().is_some() {
            anyhow::bail!("exact cmux surface identity is ambiguous");
        }
        let uuid = surface
            .uuid
            .filter(|uuid| !uuid.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("exact cmux surface has no stable UUID"))?;
        Ok(uuid)
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
            .bounded_transaction_output(&[
                "send",
                "--workspace",
                workspace_ref,
                "--surface",
                surface_ref,
                text,
            ])
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
    pub async fn new_surface(&self, workspace_ref: &str, surface_type: &str) -> Result<String> {
        let output = self
            .bounded_transaction_output(&[
                "new-surface",
                "--type",
                surface_type,
                "--workspace",
                workspace_ref,
            ])
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

/// Owns a pipe reader so cancelling the command future cannot detach the task.
struct BoundedReadTask(JoinHandle<std::io::Result<(Vec<u8>, bool)>>);

impl BoundedReadTask {
    fn new(task: JoinHandle<std::io::Result<(Vec<u8>, bool)>>) -> Self {
        Self(task)
    }

    fn abort(&self) {
        self.0.abort();
    }

    fn handle_mut(&mut self) -> &mut JoinHandle<std::io::Result<(Vec<u8>, bool)>> {
        &mut self.0
    }
}

impl Drop for BoundedReadTask {
    fn drop(&mut self) {
        self.abort();
    }
}

async fn read_bounded_output(
    mut reader: impl AsyncRead + Unpin,
    max_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut overflow = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(retained.len());
        let keep = remaining.min(count);
        retained.extend_from_slice(&chunk[..keep]);
        overflow |= keep < count;
    }
    Ok((retained, overflow))
}

async fn collect_bounded_output(
    stdout_task: &mut BoundedReadTask,
    stderr_task: &mut BoundedReadTask,
    drain_timeout: Duration,
) -> Result<((Vec<u8>, bool), (Vec<u8>, bool))> {
    let joined = timeout(drain_timeout, async {
        let stdout = stdout_task
            .handle_mut()
            .await
            .context("bounded cmux stdout reader task failed")??;
        let stderr = stderr_task
            .handle_mut()
            .await
            .context("bounded cmux stderr reader task failed")??;
        Ok::<_, anyhow::Error>((stdout, stderr))
    })
    .await;
    match joined {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            stdout_task.abort();
            stderr_task.abort();
            Err(error)
        }
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            anyhow::bail!("cmux output pipe drain timed out");
        }
    }
}

fn current_window_ref(
    parsed: &TreeJson,
    expected_window_ref: Option<&str>,
) -> Result<Option<String>> {
    if let Some(expected) = expected_window_ref {
        if parsed
            .windows
            .iter()
            .any(|window| window.ref_id == expected)
        {
            return Ok(Some(expected.to_string()));
        }
        anyhow::bail!("cmux tree did not contain expected window ref {expected}");
    }
    Ok(parsed
        .windows
        .iter()
        .find(|window| window.current || window.active || window.key)
        .or_else(|| parsed.windows.first())
        .map(|window| window.ref_id.clone()))
}

#[cfg(test)]
mod bounded_transaction_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    fn executable(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn test_client(path: &std::path::Path) -> CmuxClient {
        CmuxClient::new_with_transaction_limits(
            path.to_string_lossy().into_owned(),
            path.with_extension("sock"),
            Duration::from_secs(5),
            Duration::from_millis(20),
            1024,
        )
    }

    #[tokio::test]
    async fn every_dispatch_cmux_command_has_a_wall_clock_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("cmux");
        executable(&bin, "#!/bin/sh\nsleep 5\n");
        let client = CmuxClient::new_with_transaction_limits(
            bin.to_string_lossy().into_owned(),
            bin.with_extension("sock"),
            Duration::from_millis(20),
            Duration::from_millis(20),
            1024,
        );
        let started = Instant::now();

        assert!(client.new_surface("workspace:1", "terminal").await.is_err());
        assert!(
            client
                .exact_surface_uuid("window:1", "workspace:1", "surface:1")
                .await
                .is_err()
        );
        assert!(
            client
                .send_text("workspace:1", "surface:1", "attach\r")
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn descendant_retained_pipe_is_aborted_after_direct_cmux_exit() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("cmux");
        executable(
            &bin,
            "#!/usr/bin/env python3\nimport os, time\nif os.fork() == 0:\n    time.sleep(5)\n    os._exit(0)\nprint('OK surface:42 pane:1 workspace:1', flush=True)\nos._exit(0)\n",
        );
        let client = test_client(&bin);
        let started = Instant::now();

        let error = format!(
            "{:#}",
            client
                .new_surface("workspace:1", "terminal")
                .await
                .unwrap_err()
        );
        assert!(error.contains("output pipe drain timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn transaction_output_is_capped_before_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("cmux");
        executable(&bin, "#!/bin/sh\nprintf '%2048s' '' | tr ' ' x\n");
        let error = format!(
            "{:#}",
            test_client(&bin)
                .new_surface("workspace:1", "terminal")
                .await
                .unwrap_err()
        );
        assert!(
            error.contains("output exceeded the safety limit"),
            "{error}"
        );
    }
}
