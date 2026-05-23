use crate::mc_data::prompts::{self, PromptRules};
use anyhow::Result;
use chrono::Local;

pub fn run(project: &str, rule_id: &str) -> Result<()> {
    let mut rules = PromptRules::load(project)?;
    let mut found = false;
    let today = Local::now().format("%Y-%m-%d").to_string();
    for r in rules.active.iter_mut() {
        if prompts::rule_id(&r.pattern) == rule_id {
            r.last_fired = Some(today.clone());
            r.hits += 1;
            found = true;
            break;
        }
    }
    if found {
        rules.save()?;
    }
    Ok(())
}
