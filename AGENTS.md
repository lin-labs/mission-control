# Mission Control

Rust TUI dashboard for monitoring cmux workspaces.

## Build

After any code change, run:

```
cargo build --release
```

This updates the `mc` command globally via symlink (`~/.cargo/bin/mission-control` -> `target/release/mission-control`).
