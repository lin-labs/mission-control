# Mission Control

Rust TUI dashboard for monitoring cmux workspaces.

## Build

After any code change, run:

```
cargo build --release
```

This updates the `mc` command globally via symlink (`~/.cargo/bin/mission-control` -> `target/release/mission-control`).

## Beads Issue Tracker

This project uses Beads (`bd`) for durable issue tracking.

- Use `bd` for task tracking; do not add markdown TODO files.
- For substantive work, create or claim a Beads issue before editing and close
  it after validation.
- Keep `.beads/issues.jsonl` exported with the work when issues change.
