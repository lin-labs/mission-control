# Mission Control

Mission Control is a Rust TUI for monitoring `cmux` workspaces and answering the
only question that matters at a glance: is the agent still working, or is it
waiting on you?

It watches local and remote workspaces, summarizes recent session activity,
surfaces likely next steps, and can classify remote terminal screens when text
signals are weak.

## Features

- Sidebar view of all visible `cmux` workspaces
- Detail pane with current trajectory and suggested next steps
- Session-history watching for local agent summaries
- Remote screen preview and status classification
- Optional OpenAI or Codex-backed summarization

## Requirements

- Rust toolchain
- `cmux`
- Optional: `codex` on your `PATH` for local summarization
- Optional: `OPENAI_API_KEY` for OpenAI-backed summaries
- Optional: `TYPESAFE_API_KEY` for screen classification

## Build

```bash
cargo build --release
```

The repo is set up so the built binary is available globally as `mc`.

## Run

```bash
mc
```

Useful flags:

```bash
mc --cmux-bin cmux --cmux-socket "$CMUX_SOCKET_PATH"
mc --use-codex false --openai-api-key "$OPENAI_API_KEY"
mc --typesafe-api-key "$TYPESAFE_API_KEY"
```

## Subcommands

By default, `mc` launches the TUI. There are also non-TUI subcommands:

- `mc resolve <workspace-uuid>` — print the local data dir path for a workspace.
- `mc setup` — one-time setup: creates `~/data/mission-control/` and its `.data/` + `.archived/` subdirs.

Run `mc --help` to see all subcommand flags.

## Controls

- `j` / `k` or arrow keys: move between workspaces
- `Enter`, `l`, or right arrow: focus the detail pane
- `h`, left arrow, or `Esc`: return to the sidebar
- `s`: refresh the current screen preview
- `n`: open notes for the selected workspace
- `q`: quit

## Data storage

mc maintains a workspace data layout on disk that the TUI provisions
automatically on each refresh:

```
~/data/mission-control/
  <workspace-name>          # display alias (symlink) for the workspace
  .data/<workspace-uuid>/   # actual data dir (UUID-keyed; never moves)
    trajectory.md           # the live 3-section trajectory doc (when present)
    histories/              # snapshots written on each Esc-save
    inputs/                 # user-context notes per save
    events.jsonl            # typed action log
    surfaces/               # per-surface logs and session pointers
    name, project           # current display name / project for the workspace
  .archived/                # dismissed workspaces (Phase 5)
```

Workspace renames in cmux atomically `mv` the display symlink — the UUID-keyed
data dir never moves. If you want to bootstrap the data root explicitly,
run `mc setup` once.

## Trajectory doc (experimental)

If a workspace has a `trajectory.md` file in its data dir, mc-tui renders
it in the detail pane in place of the legacy view. The doc has three
sections: `## Goal`, `## Current surfaces`, `## Tasks & Progress`.

For now, this is read-only — editing inside the TUI lands in Phase 1b.
You can hand-edit `trajectory.md` in your editor and mc-tui will pick up
the changes on the next refresh (~30s).

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the internal design and data flow.
