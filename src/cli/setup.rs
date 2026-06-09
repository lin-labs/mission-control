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

    // Histories are owned by the agents blueprint: `install/link-obs.sh` wires
    // ~/agents/histories -> obs/Agents/Sessions (the Obsidian vault), and mc
    // only *reads* session logs from there. mc must never create or repoint
    // this path — if it is missing, point the user at the blueprint installer.
    let home = dirs::home_dir().expect("home dir");
    let sessions_dir = crate::mc_data::paths::agent_histories_dir();
    if sessions_dir.is_dir() {
        // Per-tool convenience links into the blueprint histories dir.
        for tool_history in [
            home.join(".claude/histories"),
            home.join(".codex/histories"),
        ] {
            install_history_symlink(&tool_history, &sessions_dir, &mut created)?;
        }
    } else {
        eprintln!(
            "warn: {} is missing or not a directory — session history will not load.\n      \
             The agents blueprint owns this path; run install/link-obs.sh so it resolves to \
             obs/Agents/Sessions. mc will not create or repoint it.",
            sessions_dir.display()
        );
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

    print_hook_recommendations();

    Ok(())
}

fn print_hook_recommendations() {
    println!();
    println!("mc setup: optional Claude Code SessionStart hook");
    println!();
    println!("Add to ~/.claude/settings.json under \"hooks\":");
    println!(
        r#"{{
  "SessionStart": [{{
    "type": "command",
    "command": "if [ -n \"$MC_WORKSPACE_ID\" ]; then mc bind \"$MC_SURFACE_ID\" --session-file \"$CLAUDE_SESSION_FILE\"; fi"
  }}]
}}"#
    );

    println!();
    println!("mc setup: optional shell precmd block for ~/.zshrc");
    println!();
    println!(
        r#"# >>> mc-trajectory >>>
if [ -n "$MC_WORKSPACE_ID" ]; then
  __mc_dir() {{ mc resolve "$MC_WORKSPACE_ID" 2>/dev/null; }}
  __mc_log_precmd() {{
    local rc=$?
    local dir; dir="$(__mc_dir)" || return 0
    [ -n "$dir" ] && printf '%s\t%d\t%s\t%s\n' \
      "$(date -Iseconds)" "$rc" "$PWD" "$_" \
      >> "$dir/surfaces/$MC_SURFACE_ID.log" 2>/dev/null
  }}
  precmd_functions+=(__mc_log_precmd)

  __mc_log_exit() {{
    local dir; dir="$(__mc_dir)" || return 0
    [ -n "$dir" ] && printf '%s\tEXIT\n' "$(date -Iseconds)" \
      >> "$dir/surfaces/$MC_SURFACE_ID.log" 2>/dev/null
  }}
  zshexit_functions+=(__mc_log_exit)
fi
# <<< mc-trajectory <<<"#
    );
    println!();
    println!(
        "The shell block is guarded by MC_WORKSPACE_ID so it is silent outside cmux workspaces."
    );
}

fn ensure_dir(p: &std::path::Path) -> Result<bool> {
    if p.is_dir() {
        return Ok(false);
    }
    fs::create_dir_all(p).with_context(|| format!("mkdir {p:?}"))?;
    Ok(true)
}

fn install_history_symlink(
    tool_history: &std::path::Path,
    sessions_dir: &std::path::Path,
    created: &mut Vec<String>,
) -> Result<()> {
    use std::os::unix::fs::symlink;
    if let Ok(existing) = std::fs::read_link(tool_history) {
        if existing == sessions_dir {
            return Ok(()); // already correct
        }
        std::fs::remove_file(tool_history)
            .with_context(|| format!("remove stale symlink {tool_history:?} -> {existing:?}"))?;
        symlink(sessions_dir, tool_history)
            .with_context(|| format!("symlink {tool_history:?} -> {sessions_dir:?}"))?;
        created.push(format!(
            "Updated symlink {} -> {}",
            tool_history.display(),
            sessions_dir.display()
        ));
        return Ok(());
    }
    if tool_history.exists() {
        eprintln!(
            "warn: {} exists and is not a symlink; refusing to overwrite",
            tool_history.display()
        );
        return Ok(());
    }
    if let Some(parent) = tool_history.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent of {tool_history:?}"))?;
    }
    symlink(sessions_dir, tool_history)
        .with_context(|| format!("symlink {tool_history:?} -> {sessions_dir:?}"))?;
    created.push(format!(
        "Symlinked {} -> {}",
        tool_history.display(),
        sessions_dir.display()
    ));
    Ok(())
}
