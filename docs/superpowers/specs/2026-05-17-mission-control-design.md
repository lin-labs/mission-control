# Mission Control — Design Spec

A Rust TUI for centralized monitoring and context-switching across cmux workspaces and agent sessions.

## Problem

Multiple cmux workspaces run concurrent agent sessions (Claude Code, Codex, etc.) across local and remote machines. There's no single view showing what each workspace is doing, its trajectory, or likely next steps. Context-switching requires remembering state across tabs.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                   mission-control TUI                │
│  ┌──────────┐  ┌──────────────────────────────────┐  │
│  │ Sidebar  │  │          Detail Pane              │  │
│  │          │  │  - Trajectory (LLM one-liner)     │  │
│  │ ws1 ●    │  │  - Session bullets                │  │
│  │ ws2 ○    │  │  - Next steps (LLM checkboxes)   │  │
│  │ ws3 ●    │  │  - Screen preview (read-screen)  │  │
│  │ ws4 ○    │  │  - Last activity timestamp        │  │
│  └──────────┘  └──────────────────────────────────┘  │
└────────┬──────────────────┬─────────────┬───────────┘
         │                  │             │
    ┌────▼────┐    ┌───────▼──────┐  ┌───▼──────────┐
    │  cmux   │    │  Session     │  │   OpenAI     │
    │  events │    │  files       │  │   gpt-5.0    │
    │  stream │    │  (iCloud)    │  │   summaries  │
    └─────────┘    └──────────────┘  └──────────────┘
```

Three input channels:
1. `cmux events --reconnect` — real-time tool activity for local sessions
2. Session files in `~/agents/histories/` (iCloud-synced) — canonical state for all sessions including remote
3. `cmux tree` / `cmux list-workspaces` — workspace structure (polled on startup + periodically)

One output channel: updated session files with enriched frontmatter + LLM trajectory/next-steps.

## Session File Format (Enhanced)

Existing format preserved, new fields added:

```markdown
---
date: 2026-05-17
start: 12:04
topic: mission-control-build
workspace_id: F994F691-4859-44AF-9341-D9D41723C5F5
conversation_id: 6c8707cf-bccf-420b-b42b-9cde46701342
agent: claude
host: mbp
status: active
---

- Scaffolded Rust project with ratatui TUI
- Integrated cmux events stream for workspace tracking

## Trajectory
> Building mission-control TUI for cmux workspace monitoring. Core done, wiring LLM next.

## Next Steps
- [ ] Wire up event-driven summary refresh
- [ ] Add screen preview to detail pane
```

Field semantics:
- `workspace_id` — links session to cmux workspace
- `conversation_id` — links to the JSONL transcript file
- `agent` — claude, codex, opencode, etc.
- `host` — read from `~/agents/.device` on the machine running the agent (e.g., `mbp`, `lab`, `devbox-1`)
- `status` — `active` (agent running), `idle` (no recent activity), `waiting` (needs input), `done` (session ended)
- `## Trajectory` — LLM-generated one-liner, updated on each summarization pass
- `## Next Steps` — LLM-inferred from recent conversation, checkbox format

## Hook: mission-control-hook.sh

Lives in `~/Projects/agents/configs/hooks/mission-control-hook.sh` and is registered in both Claude Code and Codex hook configurations (via AGENTS.md / settings.json / codex equivalents).

Installed on `SessionStart` and `Stop` events for both agents.

On `SessionStart`:
1. Read `~/agents/.device` for host identifier
2. Read `$CMUX_WORKSPACE_ID` env var (when available — may be absent on remote/non-cmux terminals)
3. Determine conversation ID from the hook's JSON stdin (session_id field) or from the JSONL filename
4. Find or create the session history file for this session
5. Stamp frontmatter with `workspace_id`, `conversation_id`, `agent` (claude/codex), `host`, `status: active`

On `Stop`:
1. Update session file `status` to `done`

The hook is a lightweight shell script — no LLM calls, just frontmatter updates.
The hook does NOT live in this project. It lives in the shared agent configs repo and is installed into each agent's hook pipeline.

## TUI Design

### Sidebar (~30 columns)

Lists all cmux workspaces. Each row:
- Status dot: `●` active, `◐` idle, `○` no agent, `⚠` waiting
- Workspace name
- Host badge if remote (e.g., `[lab]`)

Sorted: active agents first, then idle, then plain terminals.
Navigate with `j`/`k` or arrow keys.

### Detail Pane

For the selected workspace:
- **Header**: workspace name, agent type, host, status, last activity
- **Trajectory**: one-liner summary
- **Bullets**: session file bullet points
- **Next Steps**: checkbox items
- **Screen Preview**: last 10 lines from `cmux read-screen` (on-demand, not continuous)

For non-agent workspaces: header + screen preview only.

### Keybindings

| Key     | Action                                          |
|---------|-------------------------------------------------|
| `j`/`k` | Navigate sidebar                                |
| `Enter` | `cmux select-workspace` — jump to workspace    |
| `r`     | Force refresh: re-read session + LLM resummarize |
| `s`     | Toggle screen preview                           |
| `Tab`   | Cycle focus between sidebar and detail          |
| `q`     | Quit                                            |

## LLM Summarization

### Trigger (local sessions)
- Subscribe to `cmux events --reconnect`
- Count tool calls per `session_id`
- Every 10 tool calls (configurable), trigger summarization

### Trigger (remote sessions)
- fsnotify watches `~/agents/histories/` for file changes
- On change, re-read the session file
- Trigger LLM only if the bullets section changed since last summary

### Summarization Flow
1. Read last ~50 entries from the conversation JSONL (for local sessions) or the session file bullets (for remote)
2. Send to gpt-5.0 with a prompt requesting: one-line trajectory summary + 3-5 next-step checkboxes
3. Write `## Trajectory` and `## Next Steps` sections back to the session file
4. The prompt format is kept in `config.rs` so it can be iterated on without code changes

### LLM Abstraction
```rust
#[async_trait]
trait Summarizer {
    async fn summarize(&self, context: &str) -> Result<Summary>;
}

struct Summary {
    trajectory: String,
    next_steps: Vec<String>,
}
```

One implementation: `OpenAISummarizer` using gpt-5.0. Swappable to Anthropic, local models, etc.

## Crate Stack

| Crate       | Purpose                                    |
|-------------|--------------------------------------------|
| `ratatui`   | TUI rendering                              |
| `crossterm` | Terminal backend                           |
| `tokio`     | Async runtime for events + LLM calls       |
| `reqwest`   | OpenAI HTTP calls                          |
| `serde`     | JSON/YAML parsing                          |
| `serde_json`| cmux NDJSON events, JSONL transcripts      |
| `serde_yaml`| Session file frontmatter                   |
| `notify`    | fsnotify for session file changes          |
| `clap`      | CLI arguments                              |

## Project Structure

```
~/Tools/mission-control/
├── Cargo.toml
├── src/
│   ├── main.rs              # entry, tokio runtime, event loop
│   ├── tui/
│   │   ├── mod.rs
│   │   ├── app.rs           # app state, workspace list, selection
│   │   ├── sidebar.rs       # sidebar widget
│   │   └── detail.rs        # detail pane widget
│   ├── cmux/
│   │   ├── mod.rs
│   │   ├── client.rs        # cmux CLI calls (list-workspaces, tree, read-screen)
│   │   └── events.rs        # cmux events --reconnect NDJSON stream
│   ├── session/
│   │   ├── mod.rs
│   │   ├── file.rs          # parse/write session markdown (frontmatter + body)
│   │   └── watcher.rs       # fsnotify on ~/agents/histories/
│   ├── llm/
│   │   ├── mod.rs
│   │   └── openai.rs        # gpt-5.0 summarization behind Summarizer trait
│   └── config.rs            # paths, thresholds, model config
```

Hook lives separately at `~/Projects/agents/configs/hooks/mission-control-hook.sh`.

## Key Design Decisions

1. **Shell out to cmux CLI** rather than speaking the Unix socket — simpler, CLI is stable and well-documented.
2. **Session file is the single source of truth** — works for both local and remote (iCloud-synced) sessions. cmux events accelerate local sessions but aren't required.
3. **LLM client behind a trait** — swap OpenAI for Anthropic or local models with one impl.
4. **Host identity from `~/agents/.device`** — no hardcoded "local", each machine declares itself.
5. **Graceful degradation** — non-agent workspaces show name + screen preview. Missing fields in session files are tolerated.
6. **Prompt template in config** — the LLM summarization prompt lives in `config.rs` as a const string, easy to iterate on format without structural code changes.
