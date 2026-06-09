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
                  │ writes protocol state                 │ (no reverse channel —
                  ▼                                       │  see "Status sources"
       ┌────────────────────────────────────┐             │  below)
       │ agent native hooks                 │             │
       │   call `arcmux hook` once          │             │
       │   for prompt/tool/turn events      │             │
       └──────────────┬─────────────────────┘             │
                      ▼                                   │
       ┌────────────────────────────────────┐             │
       │ ~/data/mux/sessions/<id>.json      │             │
       │   working, last_event, last_tool,  │             │
       │   turn_count, prompt/turn times    │             │
       │   mc polls as a read-only          │             │
       │   subscriber                       │             │
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
    `classification` (TypeSafe), `mux_status`, `session`, `notes`, `loading`.
- **`tui::sidebar` / `tui::detail`** — pure render functions. Sidebar shows
  status dot or animated braille spinner when `loading == true`. Detail panel
  is organised around three questions: *what did I ask?*, *what's happening?*,
  *what should I focus on next?* (notes section is always visible).
- **`tui::app::spawn_screen_task`** — fires a `tokio::spawn` per workspace:
  `cmux read-screen` (3s timeout) → optional `TypeSafe` classify (2s timeout) →
  send a `ScreenUpdate` on the channel.
- **`tui::app::agent_state()`** — priority-ordered status derivation:
  1. mux protocol state (`~/data/mux/sessions/*.json`) once the cmux event
     stream maps `session_id` to a workspace
  2. TypeSafe classification (if confidence > 0.6)
  3. screen-insight regex (spinner with `…`/`(` vs completion `for Xm`)
  4. agent-surface fallback

### External processes
- **cmux** — the workspace daemon; mission-control is purely a client. All
  surface I/O (local pty, remote tmux over Mosh/SSH) flows through cmux. The
  remote story works precisely because mission-control never opens its own
  network connections; cmux already does.
- **Agent native hooks** — write one centralized mux protocol state doc via
  `arcmux hook`. Mission-control reads those docs; it does not install its own
  working/waiting status hook.
- **Mission-control SessionStart hook (`mc bind`)** — retained only for the
  non-overlapping session-log binding fact: cmux surface id → session-history
  file path. The mux protocol state does not carry that mapping.
- **Remote panes without local mux state** — fall back to reading the screen via
  cmux and classifying via TypeSafe.

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
| `~/data/mux/sessions/<session-id>.json` | `arcmux hook` | centralized per-agent-session activity: `working`, `last_event`, `last_tool`, `turn_count`, prompt/turn timestamps | every mux-state poll for mapped sessions | never by mc |
| `~/.config/mission-control/notes/<slug>.md` | user | persistent per-workspace notes; survives across sessions | every `refresh_workspaces` and after `n` keypress | user via `$EDITOR` (mc shells out) |
| `~/Library/Application Support/cmux/cmux.sock` | cmux | Unix socket for all subprocess calls into cmux | every `CmuxClient` call | cmux daemon |
| `target/release/mission-control` → `~/.cargo/bin/mission-control` → `mc` | cargo | the binary itself, symlinked so `cargo build --release` is enough to "deploy" globally | invoked from any shell | `cargo build --release` |

## Status detection — local vs remote

| Workspace location | Primary signal | Latency | Fallback |
|---|---|---|---|
| Local mux-spawned agents | mux session state doc | ~2s poll, immediate after cmux event | regex insights → surface detection |
| Local non-mux agents | regex insights | 15s | surface detection |
| Remote (Mosh/tmux) | TypeSafe classification of cmux-captured screen | 15s + ≤100ms | regex insights |
| No agent | surface detection | refresh tick | — |

The same `agent_state()` function handles all of them — only the order of which
source fires depends on what's available. Mission-control keeps the cmux event
subscriber only for facts the mux JSON does not include (`session_id` →
workspace_id) and for existing summary scheduling; activity facts come from the
mux files.

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
