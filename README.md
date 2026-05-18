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

## Controls

- `j` / `k` or arrow keys: move between workspaces
- `Enter`, `l`, or right arrow: focus the detail pane
- `h`, left arrow, or `Esc`: return to the sidebar
- `s`: refresh the current screen preview
- `n`: open notes for the selected workspace
- `q`: quit

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for the internal design and data flow.
