use crate::mc_data::prompts::{self, PromptRules};
use anyhow::Result;
use chrono::{Local, NaiveDate};

pub fn run() -> Result<()> {
    let root = prompts::obsagents_root().join("Projects");
    if !root.exists() {
        println!("mc gc: no obsAgents projects found at {root:?}");
        return Ok(());
    }
    let today = Local::now().naive_local().date();
    let mut total_moved = 0;
    let mut total_marked = 0;
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let project = entry.file_name().to_string_lossy().to_string();
        let mut rules = match PromptRules::load(&project) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (moved, marked) = gc_project(&mut rules, today);
        if moved > 0 || marked > 0 {
            rules.save()?;
            total_moved += moved;
            total_marked += marked;
            println!("  {project}: moved {moved} to stale, marked {marked} for review");
        }
    }
    println!(
        "mc gc: complete. {total_moved} rules moved to stale, {total_marked} flagged for deletion."
    );
    Ok(())
}

fn gc_project(rules: &mut PromptRules, today: NaiveDate) -> (usize, usize) {
    let mut moved = 0;
    let mut marked = 0;

    // Move active rules with last_fired > 30 days old to stale.
    let mut still_active = Vec::new();
    for r in rules.active.drain(..) {
        let is_stale = match &r.last_fired {
            Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map(|nd| (today - nd).num_days() > 30)
                .unwrap_or(false),
            None => false,
        };
        if is_stale {
            rules.stale.push(r);
            moved += 1;
        } else {
            still_active.push(r);
        }
    }
    rules.active = still_active;

    // Mark stale rules > 60 days for deletion (idempotent).
    for r in rules.stale.iter_mut() {
        if let Some(d) = &r.last_fired {
            if let Ok(nd) = NaiveDate::parse_from_str(d, "%Y-%m-%d") {
                if (today - nd).num_days() > 60
                    && !r.expansion.contains("# TODO: review for deletion")
                {
                    r.expansion.push_str(" # TODO: review for deletion");
                    marked += 1;
                }
            }
        }
    }

    (moved, marked)
}
