use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::process::Command;

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub ref_id: String,      // e.g. "workspace:2"
    pub uuid: String,        // e.g. "32E47B1E-..."
    pub name: String,        // e.g. "gmail-labs"
    pub selected: bool,
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

    /// Parse `cmux list-workspaces --id-format both` output.
    /// Each line: `[*] workspace:N UUID  name [selected]`
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let output = self.cmd()
            .args(["list-workspaces", "--id-format", "both"])
            .output()
            .await
            .context("failed to run cmux list-workspaces")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("cmux list-workspaces failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut workspaces = Vec::new();

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let selected = line.starts_with('*');
            let line = line.trim_start_matches('*').trim();

            // Format: "workspace:N UUID  name  [selected]"
            let parts: Vec<&str> = line.splitn(3, char::is_whitespace).collect();
            if parts.len() < 3 {
                continue;
            }
            let ref_id = parts[0].to_string();
            let uuid = parts[1].to_string();
            let name = parts[2]
                .trim()
                .trim_end_matches("[selected]")
                .trim()
                .to_string();

            workspaces.push(Workspace {
                ref_id,
                uuid,
                name,
                selected,
            });
        }

        Ok(workspaces)
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
