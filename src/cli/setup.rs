use crate::mc_data::paths;
use anyhow::{Context, Result};
use std::fs;

pub fn run() -> Result<()> {
    let root = paths::data_root();
    let data_sub = paths::data_subroot();
    let archive = paths::archive_root();

    let mut created: Vec<String> = Vec::new();
    if ensure_dir(&root)? {
        created.push(format!("Created {}", root.display()));
    }
    if ensure_dir(&data_sub)? {
        created.push(format!("Created {}", data_sub.display()));
    }
    if ensure_dir(&archive)? {
        created.push(format!("Created {}", archive.display()));
    }

    if created.is_empty() {
        println!(
            "mc setup: nothing to do — {} already exists.",
            root.display()
        );
    } else {
        println!("mc setup: complete.");
        for line in created {
            println!("  - {line}");
        }
    }
    Ok(())
}

fn ensure_dir(p: &std::path::Path) -> Result<bool> {
    if p.is_dir() {
        return Ok(false);
    }
    fs::create_dir_all(p).with_context(|| format!("mkdir {p:?}"))?;
    Ok(true)
}
