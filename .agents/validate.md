# Validate profile — mission-control

Rust TUI (`mc`) for monitoring `cmux` workspaces — answers "is the agent still
working or waiting on me?"

## Smoke command(s)

```bash
cargo build --release          # produces ./target/release/mission-control
./target/release/mission-control --help
mc                              # globally-installed binary (if installed)
```

A real boot smoke: launch `mc` against a directory with at least one cmux
workspace and confirm the TUI renders without panic. The TUI hard-exits to
the terminal, so a successful render-and-quit is the minimum E2E.

## E2E entry points

- TUI render: launch `mc` in a 120x40 terminal with a known cmux workspace
  path present; quit (`q`) cleanly without panic.
- Trajectory classification path: with `OPENAI_API_KEY` or `TYPESAFE_API_KEY`
  set, point `mc` at a workspace whose last screen is a known state (working
  vs. waiting) and assert the surfaced status matches.
- Session-history watching: drop a known summary file under the watched
  session directory and confirm `mc` picks it up live.

## Test entry points

```bash
cargo test --all-features      # unit + integration; integration tests in tests/
cargo test --test cli_smoke    # cli-level smoke
cargo test --test mc_data_*    # data layer
cargo test --test llm_*        # llm classification paths
cargo clippy -- -D warnings    # lint as errors
cargo fmt --all -- --check     # format check
```

## Fixtures and corpora

- Test fixtures live next to each `tests/<area>.rs` file or under `tests/`
  subdirectories where present.
- LLM-related tests (`llm_*`) expect mock/stub clients; do not require live
  API keys unless explicitly exercising real-deps.

## Dev environment

- Toolchain: stable Rust (`cargo`).
- Optional runtime deps: `cmux` on PATH, `codex` on PATH for local
  summarization, `OPENAI_API_KEY` / `TYPESAFE_API_KEY` for online paths.

## Known flakies and quirks

- TUI tests in a non-tty context need a pseudo-terminal; CI must allocate one.

## Highest fidelity rung available

- [x] Static / typecheck (`cargo check`, `cargo clippy`)
- [x] Unit (in-crate `#[cfg(test)]`)
- [x] Integration (`tests/*.rs`)
- [x] Real-deps E2E (binary launch against a real cmux workspace directory)
- [ ] Manual user flow (no scripted TUI screenshot diff harness yet — manual
      visual check only)

For user-visible TUI changes the gate currently tops out at "binary launches,
renders, exits cleanly" plus a manual visual check. A scripted snapshot test
(e.g. `insta` + headless terminal) would raise the top rung.
