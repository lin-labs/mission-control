use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const SECTION_MISSION: &str = "Mission";
pub const SECTION_CURRENT_SURFACES: &str = "Current surfaces";
pub const SECTION_BEADS: &str = "Beads";
/// Mission detail renders one active row plus this many history rows. Cursor
/// navigation uses the same cap so it cannot disappear into hidden history.
pub const MISSION_HISTORY_MAX_VISIBLE_ITEMS: usize = 12;
pub const MISSION_MAX_VISIBLE_ITEMS: usize = 1 + MISSION_HISTORY_MAX_VISIBLE_ITEMS;
/// Historical name kept as an API alias. The third trajectory section now
/// renders Beads issues, replacing the old hand-maintained goal/progress list.
pub const SECTION_GOALS: &str = SECTION_BEADS;

/// Legacy section names accepted by the parser for back-compat. When we
/// encounter these on read, they are normalized to the canonical new names so
/// the rest of the codebase only sees `SECTION_MISSION` / `SECTION_BEADS`.
const LEGACY_SECTION_GOAL: &str = "Goal";
const LEGACY_SECTION_GOALS_PROGRESS: &str = "Goals & Progress";
const LEGACY_SECTION_TASKS: &str = "Tasks & Progress";

pub const SECTIONS_IN_ORDER: &[&str] = &[SECTION_MISSION, SECTION_CURRENT_SURFACES, SECTION_GOALS];

/// Normalize a section header from the trajectory.md to its canonical name,
/// folding legacy headers (`Goal`, `Tasks & Progress`) onto the new taxonomy
/// (`Mission`, `Beads`).
fn canonicalize_section_name(name: &str) -> String {
    match name {
        LEGACY_SECTION_GOAL => SECTION_MISSION.to_string(),
        LEGACY_SECTION_GOALS_PROGRESS => SECTION_GOALS.to_string(),
        LEGACY_SECTION_TASKS => SECTION_GOALS.to_string(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Frontmatter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Item {
    /// Display text (no leading `- `, no checkbox marker, no HTML comment tail).
    pub text: String,
    pub is_checkbox: bool,
    pub checked: Option<bool>,
    /// Present only on items in `## Current surfaces`.
    pub surface_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default)]
pub struct TrajectoryDoc {
    pub frontmatter: Frontmatter,
    pub sections: Vec<Section>,
}

impl TrajectoryDoc {
    /// Canonical empty 3-section skeleton with frontmatter for a fresh workspace.
    pub fn skeleton(uuid: &str, name: &str, _project: &str) -> Self {
        let frontmatter = Frontmatter {
            workspace: Some(name.to_string()),
            workspace_id: Some(uuid.to_string()),
            updated: Some(chrono::Utc::now().to_rfc3339()),
            snapshot: Some(0),
        };
        let mut doc = TrajectoryDoc {
            frontmatter,
            sections: Vec::new(),
        };
        doc.ensure_sections();
        doc
    }

    pub fn parse(text: &str) -> Result<Self> {
        let (fm_str, body) = split_frontmatter(text);
        let frontmatter: Frontmatter = if fm_str.is_empty() {
            Frontmatter::default()
        } else {
            serde_yaml::from_str(fm_str).context("parse frontmatter YAML")?
        };

        let mut sections: Vec<Section> = Vec::new();
        let mut current: Option<Section> = None;
        for raw_line in body.lines() {
            let line = raw_line.trim_end();
            if let Some(name) = line.strip_prefix("## ") {
                if let Some(s) = current.take() {
                    sections.push(s);
                }
                current = Some(Section {
                    name: canonicalize_section_name(name.trim()),
                    items: Vec::new(),
                });
                continue;
            }
            if let Some(s) = current.as_mut() {
                if let Some(item) = parse_item(line, &s.name) {
                    s.items.push(item);
                }
            }
        }
        if let Some(s) = current.take() {
            sections.push(s);
        }

        Ok(TrajectoryDoc {
            frontmatter,
            sections,
        })
    }

    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// Ensure all canonical sections exist, in canonical order, creating
    /// empty ones for any missing.
    pub fn ensure_sections(&mut self) {
        let mut existing: std::collections::HashMap<String, Section> = self
            .sections
            .drain(..)
            .map(|s| (s.name.clone(), s))
            .collect();
        let mut ordered = Vec::with_capacity(SECTIONS_IN_ORDER.len());
        for canon in SECTIONS_IN_ORDER {
            let s = existing.remove(*canon).unwrap_or_else(|| Section {
                name: canon.to_string(),
                items: Vec::new(),
            });
            ordered.push(s);
        }
        // Drop any non-canonical sections silently in Phase 1a.
        self.sections = ordered;
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        // Frontmatter
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(&self.frontmatter).unwrap_or_default());
        out.push_str("---\n\n");

        for (i, section) in self.sections.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str("## ");
            out.push_str(&section.name);
            out.push('\n');
            for item in &section.items {
                out.push_str("- ");
                if item.is_checkbox {
                    out.push_str(if item.checked.unwrap_or(false) {
                        "[x] "
                    } else {
                        "[ ] "
                    });
                }
                out.push_str(&item.text);
                if let Some(sid) = &item.surface_id {
                    out.push_str(&format!("              <!-- mc:surface:{sid} -->"));
                }
                out.push('\n');
            }
        }
        out
    }

    /// Swap in a new item list for a named section. No-op if the section
    /// doesn't exist (in Phase 1a/1b, ensure_sections() always backfills, so
    /// this should never silently drop work — but be defensive).
    pub fn replace_section_items(&mut self, name: &str, items: Vec<Item>) {
        for s in self.sections.iter_mut() {
            if s.name == name {
                s.items = items;
                return;
            }
        }
    }

    pub fn sort_tasks_if_long(&mut self) {
        for s in self.sections.iter_mut() {
            if s.name != SECTION_GOALS {
                continue;
            }
            if s.items.len() <= 10 {
                return;
            }
            let with_idx: Vec<(usize, Item)> = s.items.drain(..).enumerate().collect();
            let mut todos: Vec<(usize, Item)> = Vec::new();
            let mut dones: Vec<(usize, Item)> = Vec::new();
            for (i, item) in with_idx {
                if item.is_checkbox && item.checked == Some(true) {
                    dones.push((i, item));
                } else {
                    todos.push((i, item));
                }
            }
            todos.sort_by_key(|(idx, item)| {
                let prio = priority_of(&item.text);
                // Encode: Some(n) < None. Use (0u8, n) vs (1u8, 0) for ordering.
                let prio_key = match prio {
                    Some(n) => (0u8, n),
                    None => (1u8, 0u8),
                };
                (prio_key, *idx)
            });
            // dones stay in original order — drain already preserved that, but we
            // need to sort by original idx since both vecs got idx tags.
            dones.sort_by_key(|(idx, _)| *idx);
            s.items = todos
                .into_iter()
                .chain(dones.into_iter())
                .map(|(_, item)| item)
                .collect();
            return;
        }
    }

    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir for {path:?}"))?;
        }
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, self.to_markdown()).with_context(|| format!("write tmp {tmp:?}"))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
        Ok(())
    }

    pub fn load_from_file(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut d = Self::parse(&text).with_context(|| format!("parse {path:?}"))?;
                d.ensure_sections();
                Ok(d)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut d = Self::default();
                d.ensure_sections();
                Ok(d)
            }
            Err(e) => Err(anyhow::Error::from(e).context(format!("read {path:?}"))),
        }
    }
}

pub fn priority_of(text: &str) -> Option<u8> {
    let t = text.trim_start();
    let bytes = t.as_bytes();
    if bytes.len() >= 4
        && bytes[0] == b'['
        && (bytes[1] == b'P' || bytes[1] == b'p')
        && bytes[2].is_ascii_digit()
        && bytes[3] == b']'
    {
        return Some(bytes[2] - b'0');
    }
    None
}

fn split_frontmatter(text: &str) -> (&str, &str) {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let after = &rest[end + 4..];
            // Skip the trailing newline after the closing `---` if present.
            let body = after.strip_prefix('\n').unwrap_or(after);
            return (fm, body);
        }
    }
    ("", text)
}

fn parse_item(line: &str, section_name: &str) -> Option<Item> {
    let trimmed = line.trim_start();
    let body = trimmed.strip_prefix("- ")?;

    let (text, is_checkbox, checked) = if let Some(rest) = body.strip_prefix("[x] ") {
        (rest.to_string(), true, Some(true))
    } else if let Some(rest) = body.strip_prefix("[X] ") {
        (rest.to_string(), true, Some(true))
    } else if let Some(rest) = body.strip_prefix("[ ] ") {
        (rest.to_string(), true, Some(false))
    } else {
        (body.to_string(), false, None)
    };

    // Pull HTML comment surface marker if present. A malformed comment with
    // no closing `-->` yields surface_id = None rather than a garbage id.
    let (text, surface_id) = match text.split_once("<!-- mc:surface:") {
        Some((head, tail)) => {
            let head = head.trim_end().to_string();
            let id = tail.split_once("-->").map(|(s, _)| s.trim().to_string());
            (head, id)
        }
        None => (text, None),
    };

    // For non-Current-surfaces sections, ignore any stray surface_id.
    let surface_id = if section_name == SECTION_CURRENT_SURFACES {
        surface_id
    } else {
        None
    };

    Some(Item {
        text: text.trim_end().to_string(),
        is_checkbox,
        checked,
        surface_id,
    })
}
