use crate::mc_data::paths;
use anyhow::Result;

pub fn run(workspace_id: &str) -> Result<()> {
    println!("{}", paths::workspace_dir(workspace_id).display());
    Ok(())
}
