use crate::mc_data::paths;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct InputContext {
    pub user_why: Option<String>,
    pub current_screen_tail: Option<String>,
    pub last_user_prompt: Option<String>,
    pub last_agent_output_tail: Option<String>,
    pub edited_sections: Vec<String>,
}

impl InputContext {
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("## User context\n");
        if let Some(why) = &self.user_why {
            out.push_str(&format!("why: {why}\n"));
        }
        out.push_str("\n## Auto context (captured at edit-start)\n");
        if let Some(s) = &self.current_screen_tail {
            out.push_str("current-screen-tail: |\n");
            for line in s.lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
        if let Some(s) = &self.last_user_prompt {
            // Quote the value so colons inside don't confuse YAML-ish readers.
            out.push_str(&format!("last-user-prompt: {:?}\n", s));
        }
        if let Some(s) = &self.last_agent_output_tail {
            out.push_str("last-agent-output-tail: |\n");
            for line in s.lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
        if !self.edited_sections.is_empty() {
            out.push_str(&format!(
                "edited-sections: [{}]\n",
                self.edited_sections.join(", ")
            ));
        }
        out
    }
}

pub fn write_input(uuid: &str, n: u32, ctx: &InputContext) -> Result<PathBuf> {
    let dir = paths::inputs_dir(uuid);
    fs::create_dir_all(&dir).with_context(|| format!("create {dir:?}"))?;
    let path = dir.join(format!("{n}.txt"));
    let tmp = dir.join(format!("{n}.txt.tmp"));
    fs::write(&tmp, ctx.to_text()).with_context(|| format!("write {tmp:?}"))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
    Ok(path)
}
