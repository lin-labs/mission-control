use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frontmatter {
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionFile {
    pub path: PathBuf,
    pub frontmatter: Frontmatter,
    pub bullets: Vec<String>,
    pub trajectory: Option<String>,
    pub next_steps: Vec<String>,
    pub other_body: String,
}

impl SessionFile {
    pub fn parse(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse_str(&content, path.to_path_buf())
    }

    pub fn parse_str(content: &str, path: PathBuf) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(content)?;

        let mut bullets = Vec::new();
        let mut trajectory = None;
        let mut next_steps = Vec::new();
        let mut other_body = String::new();
        let mut section: Option<&str> = None;

        for line in body.lines() {
            if line.starts_with("## Trajectory") {
                section = Some("trajectory");
                continue;
            } else if line.starts_with("## Next Steps") {
                section = Some("next_steps");
                continue;
            } else if line.starts_with("## ") {
                section = Some("other");
            }

            match section {
                None => {
                    let trimmed = line.trim();
                    if trimmed.starts_with("- ")
                        && !trimmed.starts_with("- [ ]")
                        && !trimmed.starts_with("- [x]")
                    {
                        bullets.push(trimmed.trim_start_matches("- ").to_string());
                    } else if !trimmed.is_empty() {
                        other_body.push_str(line);
                        other_body.push('\n');
                    }
                }
                Some("trajectory") => {
                    let trimmed = line.trim().trim_start_matches("> ").trim();
                    if !trimmed.is_empty() {
                        trajectory = Some(trimmed.to_string());
                    }
                }
                Some("next_steps") => {
                    let trimmed = line.trim();
                    if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") {
                        next_steps.push(trimmed.to_string());
                    }
                }
                _ => {
                    other_body.push_str(line);
                    other_body.push('\n');
                }
            }
        }

        Ok(SessionFile {
            path,
            frontmatter,
            bullets,
            trajectory,
            next_steps,
            other_body,
        })
    }

    pub fn to_markdown(&self) -> Result<String> {
        let yaml = serde_yaml::to_string(&self.frontmatter)?;
        let mut out = format!("---\n{}---\n\n", yaml);

        for bullet in &self.bullets {
            out.push_str(&format!("- {}\n", bullet));
        }

        if let Some(ref traj) = self.trajectory {
            out.push_str(&format!("\n## Trajectory\n> {}\n", traj));
        }

        if !self.next_steps.is_empty() {
            out.push_str("\n## Next Steps\n");
            for step in &self.next_steps {
                out.push_str(&format!("{}\n", step));
            }
        }

        if !self.other_body.trim().is_empty() {
            out.push('\n');
            out.push_str(self.other_body.trim());
            out.push('\n');
        }

        Ok(out)
    }

    pub fn write(&self) -> Result<()> {
        let content = self.to_markdown()?;
        std::fs::write(&self.path, content)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

fn split_frontmatter(content: &str) -> Result<(Frontmatter, String)> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return Ok((Frontmatter::default(), content.to_string()));
    }

    let after_first = &content[3..];
    let end = after_first
        .find("\n---")
        .context("no closing --- in frontmatter")?;

    let yaml_str = &after_first[..end];
    let body = &after_first[end + 4..];

    let frontmatter: Frontmatter = serde_yaml::from_str(yaml_str.trim()).unwrap_or_default();

    Ok((frontmatter, body.trim_start_matches('\n').to_string()))
}

pub fn list_session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "md")
                && p.file_name()
                    .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
        })
        .collect();

    files.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });

    Ok(files)
}
