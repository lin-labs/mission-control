# Mission Control — Architecture

Rust TUI dashboard that watches every cmux workspace (local + remote via Mosh/tmux)
and answers one question per workspace: **is the agent baking, or is it waiting for me?**

## System diagram

```
                              ┌────────────────────────────────────────────┐
                              │                  mc-tui                    │
                              │                                            │
                              │   ┌────────────────────────────────────┐   │
                              │   │  App state (tui::app::App)         │   │
                              │   │  - Vec<WorkspaceState>             │   │
                              │   │  - selected / focus / scroll       │   │
                              │   │  - workspace_index, hashes         │   │
                              │   └────────────────────────────────────┘   │
                              │           ▲          ▲          ▲          │
                              │           │          │          │          │
                              │   ┌───────┴──┐  ┌────┴────┐  ┌──┴────────┐ │
                              │   │ render   │  │ select! │  │ spawn     │ │
                              │   │ (ratatui)│  │ loop    │  │ tasks     │ │
                              │   └──────────┘  └─────────┘  └───────────┘ │
                              └────┬──────────────┬─────────────────┬──────┘
                                   │              │                 │
              ┌────────────────────┘              │                 └───────────────────────┐
              │                                   │                                         │
              ▼                                   ▼                                         ▼
  ┌──────────────────────┐         ┌──────────────────────────┐            ┌──────────────────────────┐
  │      CmuxClient      │         │   SessionWatcher (notify)│            │   Screen-update channel  │
  │   (subprocess calls) │         │ ~/agents/histories/*.md  │            │  mpsc<ScreenUpdate>      │
  │                      │         │                          │            │                          │
  │  list-workspaces     │         │  file-change events      │            │  spawned tasks per       │
  │  tree --all          │         │  → SessionFile::parse    │            │  workspace push results  │
  │  read-screen --lines │         │  → bullets, frontmatter  │            │  back here; main loop    │
  │  select-workspace    │         │                          │            │  applies them via        │
  │  events subscribe    │         └──────────────────────────┘            │  apply_screen_update     │
  └─────┬───────────┬────┘                                                 └──────────────────────────┘
        │           │
        │           │ events stream
        │           ▼
        │   ┌─────────────────────┐
        │   │ cmux events socket  │  tool-call events → tool_call_count++
        │   │  (AgentEvent)       │  → triggers OpenAI summary when > threshold
        │   └─────────────────────┘
        │
        │ subprocess via $CMUX_SOCKET_PATH
        ▼
  ┌────────────────────────────────────────────────────────────────────┐
  │                          cmux (daemon)                             │
  │                                                                    │
  │   workspace:1  arcmux           ──────────┐                        │
  │   workspace:2  gmail-labs       ─── attached tmux on remote ───┐   │
  │   workspace:3  skill-stats      ──────────┐                    │   │
  │   workspace:5  mission-control  ─── local Claude Code surface  │   │
  │                                                                │   │
  │   read-screen returns the terminal buffer of the focused       │   │
  │   surface, whether it lives in a local pty or a remote tmux    │   │
  │   session attached over Mosh.  Mission-control does not        │   │
  │   itself open SSH/Mosh connections; cmux already owns them.    │   │
  └────────────────────────────────────────────────────────────────────┘
                  │                                       │
                  │ local                                 │ remote
                  ▼                                       ▼
       ┌──────────────────┐                  ┌─────────────────────────┐
       │ local pty/tmux   │                  │  Mosh/SSH ─► remote tmux│
       │ Claude Code,     │                  │  Claude Code, Codex on  │
       │ Codex, …         │                  │  blin-labs, etc.        │
       └──────────────────┘                  └─────────────────────────┘
                  │                                       │
                  │ writes status                         │ (no reverse channel —
                  ▼                                       │  see "Status sources"
       ┌────────────────────────────────────┐             │  below)
       │ Claude Code hook                   │             │
       │   ~/.claude/settings.json triggers │             │
       │   ~/.config/mission-control/       │             │
       │     hooks/mc-status.sh             │             │
       │   on PostToolUse / Stop            │             │
       └──────────────┬─────────────────────┘             │
                      ▼                                   │
       ┌────────────────────────────────────┐             │
       │ ~/.config/mission-control/status/  │             │
       │   <workspace-uuid>.json            │             │
       │   {"state":"working|waiting",      │             │
       │    "agent":"claude","ts":...}      │             │
       │   mc reads on every workspace      │             │
       │   refresh (mtime = freshness)      │             │
       └────────────────────────────────────┘             │
                                                          │
  ┌───────────────────────────────────────────────────────┘
  │  Remote screen → spawned classifier task
  ▼
  ┌──────────────────────────────────────┐         ┌────────────────────────┐
  │   TypeSafeClassifier                 │ HTTPS   │  api.typesafe.ai       │
  │   POST /preview/evaluation           │────────►│  (≤100ms per call)     │
  │   document = screen text             │         │  returns:              │
  │   prompts:                           │         │   - chosen state       │
  │     choice  agent_state              │         │   - state confidence   │
  │     noul    has_agent                │         │   - has_agent prob.    │
  │     noul    has_user_prompt          │         │   - has_user_prompt    │
  └──────────────────────────────────────┘         └────────────────────────┘

  ┌──────────────────────────────────────┐         ┌────────────────────────┐
  │   OpenAISummarizer                   │ HTTPS   │  api.openai.com        │
  │   triggered when:                    │────────►│  /v1/chat/completions  │
  │     tool_call_count >= threshold     │         │  returns TRAJECTORY +  │
  │     OR session file changed          │         │  NEXT_STEPS bullets    │
  │     OR user pressed 'r'              │         │                        │
  │   input = session bullets joined     │         │                        │
  └──────────────────────────────────────┘         └────────────────────────┘
```

## Components

### Inside the binary
- **`tui::app::App`** — single source of truth. Holds the workspace list, the
  channel-driven status updates, and the indices for fast lookup by uuid.
  Never blocks; all I/O happens in spawned tasks.
- **`tui::app::WorkspaceState`** — per-workspace cache:
  - `screen_preview` (last 15 lines), `screen_insights` (regex-parsed),
    `classification` (TypeSafe), `hook_status`, `session`, `notes`, `loading`.
- **`tui::sidebar` / `tui::detail`** — pure render functions. Sidebar shows
  status dot or animated braille spinner when `loading == true`. Detail panel
  is organised around three questions: *what did I ask?*, *what's happening?*,
  *what should I focus on next?* (notes section is always visible).
- **`tui::app::spawn_screen_task`** — fires a `tokio::spawn` per workspace:
  `cmux read-screen` (3s timeout) → optional `TypeSafe` classify (2s timeout) →
  send a `ScreenUpdate` on the channel.
- **`tui::app::agent_state()`** — priority-ordered status derivation:
  1. hook status file (if mtime < 60s old)
  2. TypeSafe classification (if confidence > 0.6)
  3. session frontmatter `status` field
  4. screen-insight regex (spinner with `…`/`(` vs completion `for Xm`)
  5. agent-surface fallback

### External processes
- **cmux** — the workspace daemon; mission-control is purely a client. All
  surface I/O (local pty, remote tmux over Mosh/SSH) flows through cmux. The
  remote story works precisely because mission-control never opens its own
  network connections; cmux already does.
- **Claude Code (local)** — hooks call `mc-status.sh` on PostToolUse / Stop,
  giving sub-second status updates without polling.
- **Claude Code / Codex (remote)** — no reverse channel. Status comes from
  reading the screen via cmux and classifying via TypeSafe.

### External services
- **TypeSafe AI** — `api.typesafe.ai/preview/evaluation`. Non-LLM judgment
  API; structured response with confidence values. One call per workspace
  per 15s refresh, fans out to three prompts (choice + 2 nouls).
- **OpenAI** — `api.openai.com/v1/chat/completions`. Used only for
  generating session **trajectory** + **next-steps** bullets, not for
  status detection.

## Data storage

Every persistent surface lives on the local filesystem. No databases.

| Path | Owner | What | Read | Written |
|---|---|---|---|---|
| `~/agents/histories/*.md` | other tools | session files: frontmatter (workspace_id, agent, host, status, topic) + bullet log + trajectory + next_steps | every refresh + notify watcher | `OpenAISummarizer` writes back `trajectory` + `next_steps` after summarising |
| `~/.config/mission-control/notes/<slug>.md` | user | persistent per-workspace notes; survives across sessions | every `refresh_workspaces` and after `n` keypress | user via `$EDITOR` (mc shells out) |
| `~/.config/mission-control/status/<workspace-uuid>.json` | Claude hooks | `{"state":"working|waiting","agent":"claude","ts":"..."}` — instant local status without polling | every `refresh_workspaces`; file mtime treated as freshness | `~/.config/mission-control/hooks/mc-status.sh` invoked by Claude Code |
| `~/.config/mission-control/hooks/mc-status.sh` | user (one-time) | the hook script itself | n/a | created on install; user references it from `~/.claude/settings.json` |
| `~/Library/Application Support/cmux/cmux.sock` | cmux | Unix socket for all subprocess calls into cmux | every `CmuxClient` call | cmux daemon |
| `target/release/mission-control` → `~/.cargo/bin/mission-control` → `mc` | cargo | the binary itself, symlinked so `cargo build --release` is enough to "deploy" globally | invoked from any shell | `cargo build --release` |

## Status detection — local vs remote

| Workspace location | Primary signal | Latency | Fallback |
|---|---|---|---|
| Local Claude Code | hook status file | sub-second | regex insights → surface detection |
| Local Codex / others (no hook) | regex insights | 15s | TypeSafe → surface detection |
| Remote (Mosh/tmux) | TypeSafe classification of cmux-captured screen | 15s + ≤100ms | regex insights |
| No agent | surface detection | refresh tick | — |

The same `agent_state()` function handles all of them — only the order of which
source fires depends on what's available. There is no SSH tunnel and no remote
listener: cmux already exposes the remote screen, and TypeSafe + screen-reading
is enough signal to answer the only question that matters.

## Concurrency model

```
main loop ─────────► tokio::select! {
                         tokio::time::sleep(50ms)    → poll keys, redraw  (spinner animates here)
                         refresh_interval (30s)     → refresh workspaces synchronously (cheap: list + tree + fs)
                         screen_interval (15s)      → spawn N parallel screen tasks  (NEVER awaited)
                         event_rx                   → cmux tool-call event   (increments tool_call_count)
                         file_rx                    → session file change    (re-parse + maybe summarise)
                         summary_rx                 → OpenAI summary done    (apply + write back to session file)
                         screen_rx                  → ScreenUpdate arrives  (apply_screen_update → unset loading)
                     }
```

Every long-running I/O (read-screen, TypeSafe, OpenAI, `select-workspace`,
editor for notes) is moved off the main loop. The UI redraws every 50ms, so
spinners animate even while everything else is in flight, and a single hung
remote workspace never freezes anything else.
