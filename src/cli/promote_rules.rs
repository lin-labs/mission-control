use crate::mc_data::prompts::{self, PromptRules};
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(proposals_file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(proposals_file)
        .with_context(|| format!("read {proposals_file:?}"))?;
    let ticked = prompts::parse_proposal_file(&text)?;
    if ticked.is_empty() {
        println!("mc promote-rules: no ticked rules in {proposals_file:?}");
        return Ok(());
    }
    // Derive project from proposals_file path:
    // .../Projects/<project>/prompts/proposals/<file>
    let project = infer_project(proposals_file)?;
    let mut rules = PromptRules::load(&project)?;
    for r in &ticked {
        rules.active.push(r.clone());
    }
    rules.save()?;
    // Move proposals file to .archived/
    let archived = proposals_file
        .parent()
        .context("proposals file has no parent dir")?
        .join(".archived");
    std::fs::create_dir_all(&archived)?;
    let dest = archived.join(
        proposals_file
            .file_name()
            .context("proposals file has no filename")?,
    );
    std::fs::rename(proposals_file, &dest)?;
    println!(
        "mc promote-rules: promoted {} rules to {}/prompts/rules.md",
        ticked.len(),
        project
    );
    Ok(())
}

fn infer_project(p: &Path) -> Result<String> {
    // .../Projects/<project>/prompts/proposals/X.md → <project>
    let components: Vec<_> = p.components().collect();
    for (i, comp) in components.iter().enumerate() {
        let s = comp.as_os_str().to_string_lossy();
        if s == "Projects" {
            if let Some(next) = components.get(i + 1) {
                return Ok(next.as_os_str().to_string_lossy().to_string());
            }
        }
    }
    anyhow::bail!("could not infer project from path {p:?}")
}
