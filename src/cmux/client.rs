use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use tokio::process::Command;

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub ref_id: String,      // e.g. "workspace:2"
    pub uuid: String,        // e.g. "32E47B1E-..."
    pub name: String,        // e.g. "gmail-labs"
    pub selected: bool,
}

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
}
