use crate::cmux::client::{CmuxClient, Workspace};
use crate::cmux::events::AgentEvent;
use crate::llm::Summary;
use crate::session::file::{self, SessionFile};
use crate::session::watcher::FileChanged;
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WorkspaceState {
    pub workspace: Workspace,
    pub session: Option<SessionFile>,
    pub screen_preview: Option<String>,
    pub tool_call_count: u32,
    pub show_screen: bool,
}

pub struct App {
    pub workspaces: Vec<WorkspaceState>,
    pub selected: usize,
    pub should_quit: bool,
    session_to_workspace: HashMap<String, String>,
    workspace_index: HashMap<String, usize>,
    bullet_hashes: HashMap<PathBuf, u64>,
}

impl App {
    pub fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            selected: 0,
            should_quit: false,
            session_to_workspace: HashMap::new(),
            workspace_index: HashMap::new(),
            bullet_hashes: HashMap::new(),
        }
    }

    pub async fn refresh_workspaces(
        &mut self,
        client: &CmuxClient,
        histories_dir: &std::path::Path,
    ) -> Result<()> {
        let workspaces = client.list_workspaces().await?;
        let session_files = file::list_session_files(histories_dir).unwrap_or_default();

        let mut sessions_by_workspace: HashMap<String, SessionFile> = HashMap::new();
        for path in &session_files {
            if let Ok(sf) = SessionFile::parse(path) {
                if let Some(ref ws_id) = sf.frontmatter.workspace_id {
                    sessions_by_workspace.entry(ws_id.clone()).or_insert(sf);
                }
            }
        }

        let old_counts: HashMap<String, u32> = self
            .workspaces
            .iter()
            .map(|ws| (ws.workspace.uuid.clone(), ws.tool_call_count))
            .collect();

        self.workspaces = workspaces
            .into_iter()
            .map(|ws| {
                let session = sessions_by_workspace.remove(&ws.uuid);
                let tool_call_count = old_counts.get(&ws.uuid).copied().unwrap_or(0);
                WorkspaceState {
                    workspace: ws,
                    session,
                    screen_preview: None,
                    tool_call_count,
                    show_screen: false,
                }
            })
            .collect();

        self.workspaces.sort_by(|a, b| {
            let a_active = a.session.as_ref().is_some_and(|s| {
                s.frontmatter.status.as_deref() == Some("active")
            });
            let b_active = b.session.as_ref().is_some_and(|s| {
                s.frontmatter.status.as_deref() == Some("active")
            });
            b_active
                .cmp(&a_active)
                .then_with(|| a.workspace.name.cmp(&b.workspace.name))
        });

        self.workspace_index.clear();
        for (i, ws) in self.workspaces.iter().enumerate() {
            self.workspace_index.insert(ws.workspace.uuid.clone(), i);
        }

        Ok(())
    }

    pub fn handle_agent_event(&mut self, event: &AgentEvent) {
        self.session_to_workspace
            .insert(event.session_id.clone(), event.workspace_id.clone());

        if let Some(&idx) = self.workspace_index.get(&event.workspace_id) {
            self.workspaces[idx].tool_call_count += 1;
        }
    }

    pub fn needs_summary(&self, workspace_uuid: &str, threshold: u32) -> bool {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            let ws = &self.workspaces[idx];
            ws.tool_call_count >= threshold && ws.session.is_some()
        } else {
            false
        }
    }

    pub fn reset_tool_count(&mut self, workspace_uuid: &str) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            self.workspaces[idx].tool_call_count = 0;
        }
    }

    pub fn apply_summary(&mut self, workspace_uuid: &str, summary: Summary) {
        if let Some(&idx) = self.workspace_index.get(workspace_uuid) {
            if let Some(ref mut session) = self.workspaces[idx].session {
                session.trajectory = Some(summary.trajectory);
                session.next_steps = summary.next_steps;
            }
        }
    }

    pub fn handle_file_changed(&mut self, changed: &FileChanged) -> Option<String> {
        let sf = SessionFile::parse(&changed.path).ok()?;
        let ws_id = sf.frontmatter.workspace_id.clone()?;

        let new_hash = hash_bullets(&sf.bullets);
        let old_hash = self.bullet_hashes.get(&changed.path).copied();
        self.bullet_hashes.insert(changed.path.clone(), new_hash);
        let bullets_changed = old_hash.is_some_and(|h| h != new_hash);

        if let Some(&idx) = self.workspace_index.get(&ws_id) {
            self.workspaces[idx].session = Some(sf);
        }

        if bullets_changed {
            Some(ws_id)
        } else {
            None
        }
    }

    pub fn selected_workspace(&self) -> Option<&WorkspaceState> {
        self.workspaces.get(self.selected)
    }

    pub fn workspace_index_for(&self, uuid: &str) -> Option<&usize> {
        self.workspace_index.get(uuid)
    }

    pub fn next(&mut self) {
        if !self.workspaces.is_empty() {
            self.selected = (self.selected + 1) % self.workspaces.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.workspaces.is_empty() {
            self.selected = (self.selected + self.workspaces.len() - 1) % self.workspaces.len();
        }
    }
}

fn hash_bullets(bullets: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bullets.hash(&mut hasher);
    hasher.finish()
}
