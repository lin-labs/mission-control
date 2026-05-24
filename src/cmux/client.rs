use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::process::Command;

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
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub ref_id: String,      // e.g. "workspace:2"
    pub uuid: String,        // e.g. "32E47B1E-..."
    pub name: String,        // e.g. "gmail-labs"
    pub selected: bool,
    /// The cmux workspace description (from `cmux workspace-action set-description`).
    /// Non-empty description is used to seed the Goal section of the trajectory doc.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SurfaceInfo {
    pub title: String,
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
        let output = self.cmd()
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

    /// Read the last N lines of a surface's screen.
    pub async fn read_screen(
        &self,
        workspace_ref: &str,
        lines: u32,
    ) -> Result<String> {
        let output = self.cmd()
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

    /// Select a workspace (focus it).
    pub async fn select_workspace(&self, workspace_ref: &str) -> Result<()> {
        self.cmd()
            .args(["select-workspace", "--workspace", workspace_ref])
            .output()
            .await
            .context("failed to run cmux select-workspace")?;
        Ok(())
    }

    /// Parse `cmux tree --all` to get surface titles per workspace ref.
    pub async fn get_surfaces(&self) -> Result<HashMap<String, Vec<SurfaceInfo>>> {
        let output = self.cmd()
            .args(["tree", "--all"])
            .output()
            .await
            .context("failed to run cmux tree --all")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut result: HashMap<String, Vec<SurfaceInfo>> = HashMap::new();
        let mut current_ws_ref: Option<String> = None;

        for line in stdout.lines() {
            if let Some(pos) = line.find("workspace workspace:") {
                let after = &line[pos + "workspace ".len()..];
                // after = "workspace:5 \"mission-control\""
                let ref_id = after.split_whitespace().next().unwrap_or("");
                if !ref_id.is_empty() {
                    current_ws_ref = Some(ref_id.to_string());
                }
            } else if let Some(pos) = line.find("surface surface:") {
                if let Some(ref ws_ref) = current_ws_ref {
                    let after = &line[pos..];
                    if let Some(title) = extract_quoted_title(after) {
                        result
                            .entry(ws_ref.clone())
                            .or_default()
                            .push(SurfaceInfo { title });
                    }
                }
            }
        }

        Ok(result)
    }
}

/// Extract the first quoted string from a line.
fn extract_quoted_title(line: &str) -> Option<String> {
    let first_quote = line.find('"')?;
    let rest = &line[first_quote + 1..];
    let end_quote = rest.find('"')?;
    Some(rest[..end_quote].to_string())
}
