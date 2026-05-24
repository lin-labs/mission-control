# Validate profile — mission-control

Rust TUI (`mc`) that monitors `cmux` workspaces and answers "is the agent
still working or waiting on me?" — plus a structured trajectory doc per
workspace with editing, peek-into-surface, LLM regen, and session-log
integration. This file tells the `validate` skill (and any agent doing work
on this repo) **what "done" actually means here** and **the specific failure
modes that have bitten us before** so they get caught in the next iteration,
not the one after that.

If a change shipped without satisfying these gates, treat it as "not done"
regardless of what the subagent's self-report said.

---

## The five-tier validation gate (every feature dispatch must clear them in order)

Tier-skipping is the source of every "this doesn't work" turn in this repo's
history. Do not declare success at tier N until tier N-1 is genuinely green.

### Tier 1 — Compile cleanly, no new warnings

```bash
cargo build 2>&1 | tee /tmp/mc-build.log
WARN_COUNT=$(grep -c '^warning:' /tmp/mc-build.log)
echo "warnings: $WARN_COUNT"
```

**Pass criteria**: zero errors, `$WARN_COUNT` ≤ the current baseline (see
"Warning budget" below). New warnings are blockers, even if the feature
otherwise works — they pile up and hide real signal.

### Tier 2 — All tests green, single-threaded

```bash
cargo test -- --test-threads=1
```

Many tests mutate `HOME` / `OBS_AGENTS` env vars via the panic-safe
`with_tmp_home` / `with_tmp_obs` helpers and **require single-threaded
execution**. Parallel runs will flake. Always use `--test-threads=1`.

**Pass criteria**: all tests pass; zero failures; new tests added for the new
behavior. The test count must increase by the number of new behaviors added
(typically 3–8 per feature).

### Tier 3 — CLI / data-layer smoke

For any change to `src/cli/` or `src/mc_data/`:

```bash
cargo build --release
./target/release/mission-control --help              # no env leaks, all subcmds visible
./target/release/mission-control resolve test-uuid    # prints absolute path
./target/release/mission-control setup                # creates ~/data/mission-control/, idempotent
```

**Pass criteria**: every new subcommand appears in `--help`, no API-key values
appear in `--help` output (see "Secret-leak guard" below), `mc resolve <id>`
returns the expected path string, `mc setup` is idempotent on a second run.

### Tier 4 — Live binary against real cmux (the gate that has saved us repeatedly)

**This is the most important gate. Skipping it is the most common reason a
"DONE" claim later turns out to be broken.**

```bash
cargo build --release
mc --help                # via the symlinked global binary
mc                       # launch the TUI against the user's real cmux state
```

For UI changes, manually verify:

- TUI starts without panic.
- Sidebar lists the real cmux workspaces (count matches `cmux list-workspaces`).
- Selecting a workspace renders the new 3-section trajectory pane.
- The specific behavior under test works end-to-end with real input.
- `q` exits cleanly.

For non-UI changes (CLI, data layer): the prior tiers suffice. For ANY
change that touches `src/cmux/`, `src/tui/`, `src/main.rs`, or the regen /
peek / dismissal pipelines: tier 4 is mandatory before declaring done.

Auto-graders cannot fully exercise this tier — the test framework can launch
the TUI under `TestBackend` (see `tests/tui_trajectory_render.rs`,
`tests/peek_agent.rs`), but the live integration with `cmux` is the actual
target. If you cannot run the live launch, **state that explicitly in the
report**; do not silently pretend tier 4 passed.

### Tier 5 — Cross-workspace / cross-project data sanity

Mc-tui is a *long-running* tool watching *multiple* workspaces. The bugs that
have cost us the most are integration-level: one workspace's tasks leaking
into another, or a misattributed session log polluting the regen prompt.

After any change to: regen inputs, session-log resolution, surface
projection, dismissal flow, or trajectory persistence — manually inspect:

```bash
# 1. Each workspace's trajectory.md is for ITS workspace (no cross-project content).
for d in ~/data/mission-control/.data/*/; do
  uuid=$(basename "$d")
  name=$(cat "$d/name" 2>/dev/null)
  echo "=== $name ($uuid) ==="
  awk '/^## Tasks & Progress/,0' "$d/trajectory.md" | head -5
done

# 2. Session logs in ~obsAgents/Sessions/ are tagged with the workspace that
#    matches their cwd (not just a workspace that hosts a surface).
for f in ~obsAgents/Sessions/*.md; do
  uuid=$(grep -m1 '^workspace_id:' "$f" | awk '{print $2}')
  cwd=$(grep -m1 '^cwd:' "$f" | sed 's/^cwd: //')
  topic=$(grep -m1 '^topic:' "$f" | sed 's/^topic: //')
  printf '%s  uuid=%s  cwd=%s  topic=%s\n' "$(basename "$f")" "$uuid" "$cwd" "$topic"
done | grep -i "<workspace-name>"
```

**Pass criteria**: no workspace's `## Tasks & Progress` contains content that
references a project outside the workspace's own cwd. No session log is tagged
with a workspace whose `current_directory` is unrelated to the log's `cwd`.

---

## Warning budget

`cargo build` baseline warning count today: **10**. Track this in the PR
description for any change. If a change pushes it past 10, either fix the new
warning or document why it's acceptable (e.g. a `pub` API item not yet wired
from the binary). Never let it drift up silently — once we tolerate 11, 12
arrives next sprint.

Recurring pre-existing warnings (do not silence with `#[allow(dead_code)]`;
they'll resolve as future tasks consume them):
- Various unused `pub` items in `src/mc_data/` exposed for integration tests
  but not yet called from the binary path.
- Fields in `src/cmux/events.rs` (`hook_event_name`, etc.) that are parsed
  for completeness but not yet read.

---

## Failure modes this repo has hit before — the specific checks that prevent them

Each one is a regression-prevention checklist item. When making any change
in the affected area, run the corresponding check.

### F1 — External JSON shape assumed wrong (cmux `uuid` vs `id`)

**Symptom**: `mc` crashes at startup with `missing field 'uuid' at line N`.
**Root cause**: code parsed `cmux list-workspaces --json` expecting a field
that only appears with `--id-format both`, and under a different name (`id`).

**Prevention checklist** for any task touching external command output
(cmux, codex CLI, git, anything):
1. Run the actual command and paste 30+ lines of its real output into the
   subagent prompt or PR description.
2. Identify every field your code reads.
3. Identify whether any field is conditional on a flag (e.g.
   `--id-format both` for the `id` field).
4. Add a test that loads a frozen fixture of the real output shape.

```bash
# Cmux JSON shape sanity (run before changing src/cmux/client.rs):
cmux list-workspaces --json --id-format both | python3 -c '
import sys, json; d = json.load(sys.stdin)
print("fields:", sorted(d["workspaces"][0].keys()))
print("uuid sample:", d["workspaces"][0]["id"])
'
```

### F2 — Integration gap (peek `-` key, `i` keystroke routing)

**Symptom**: feature compiles, unit tests pass, but the user presses the key
and nothing happens.
**Root cause**: `src/main.rs` has a fixed allowlist of keys it forwards to
`handle_trajectory_key`. New keys added to the trajectory editor must also
be added to that allowlist, OR a "route all keys when peek/insert is active"
gate must apply.

**Prevention checklist** for any new keybinding in `src/tui/trajectory_edit.rs`
or `src/tui/peek_view.rs`:
1. Add the `KeyCode` to the `is_traj_nav_key` allowlist in `src/main.rs`,
   OR confirm the broader `in_peek || in_insert` guard covers it.
2. Manually launch `mc` and press the key — confirm the visible effect.
3. Add a comment in `main.rs`'s match block referencing the new key so it's
   discoverable.

### F3 — Empty trajectory.md on every workspace (the "this doesn't work" gap)

**Symptom**: every cmux workspace shows the legacy detail pane; new trajectory
features are invisible because nothing creates the file.
**Root cause**: layered system shipped without the bootstrap step.

**Prevention checklist** for any feature that depends on a per-workspace file:
1. Confirm `src/mc_data/workspace.rs::ensure_workspace` creates the file (or
   the directory it lives in) on first refresh.
2. Tier 4 smoke: launch `mc`, switch to a workspace you've never opened in
   the new build, confirm the feature is immediately visible without manual
   setup.

### F4 — Subagent claimed DONE but wiring incomplete

**Symptom**: PR/commit reports "tests pass", but a manual smoke shows the
behavior isn't reachable from the entry point (main.rs / app.rs).
**Root cause**: subagents test in isolation; they don't always trace the
data flow end-to-end.

**Prevention checklist** for any subagent dispatched to add a feature:
Require the subagent's report to include an **end-to-end data flow trace**:

> Walk through what happens from the user's keystroke (or external event)
> through to the visible result. For each step, name the file:line that
> handles it. If the chain ends in code that doesn't exist yet, say so.

Wave 2.5 (wire trajectory key handlers) only existed because Wave 2A's report
didn't surface that the methods it added were uncalled.

### F5 — Hook script missing execute bit

**Symptom**: `/bin/sh: ~/.claude/hooks/<name>.sh: Permission denied`.
**Root cause**: `Edit`/`Write` tools preserve the existing mode (or default
to non-executable) when creating shell scripts.

**Prevention checklist** for any new file under `~/.claude/hooks/` (or
generally, any new `*.sh` referenced from settings):

```bash
chmod +x ~/.claude/hooks/<name>.sh
ls -la ~/.claude/hooks/<name>.sh   # confirm -rwxr-xr-x
```

Run after creation. Add a one-line smoke (`</dev/null <script>; echo $?`)
that exits 0 on a known-no-op invocation.

### F6 — Secret leaked in `--help`

**Symptom**: `mc --help` prints `[env: OPENAI_API_KEY=sk-...realvalue...]`.
**Root cause**: clap's `#[arg(env = "...")]` shows the resolved value by
default.

**Prevention checklist** for any new env-var-backed arg in `src/config.rs`:

```rust
#[arg(long, env = "<SECRET_NAME>", hide_env_values = true)]   // ← required
pub <field>: Option<String>,
```

Add a `cargo run --quiet -- --help | grep <SECRET_NAME>` check that
asserts the value isn't shown.

### F7 — Cross-workspace data contamination

**Symptom**: workspace A's `## Tasks & Progress` contains content that's
clearly about project B.
**Root cause**: `latest_session_file_for_workspace` matched by
`workspace_id` only. cmux workspaces can host surfaces with different cwds
(e.g. a `~/agents/skills` surface inside the `mission-control` cmux
workspace). The agent's SessionStart hook tags the log with the *cmux*
workspace_id even though the work is on a different project.

Fix landed in Phase 3 T5: host+cwd matching with workspace_id fallback.

**Prevention checklist** for any change that joins data across workspaces:
1. Read `~/Library/Mobile Documents/iCloud~md~obsidian/Documents/Agents/Sessions/`
   and confirm logs are tagged correctly (`workspace_id` matches a workspace
   whose `current_directory` is an ancestor of the log's `cwd`).
2. After change, run the tier-5 cross-workspace sanity script above.
3. New session logs MUST include `cwd:` and `host:` per AGENTS.shared.md's
   "Session History Logging" spec.

### F8 — Worktree session-state mismatch (EnterWorktree confusion)

**Symptom**: `EnterWorktree` errors "Already in a worktree session" even
after the worktree was manually removed.
**Root cause**: the harness tracks worktree-session state separately from
the filesystem. Manual `git worktree remove` doesn't update the session.

**Prevention checklist** for sprint cleanup:
- After merging a feature branch, use `ExitWorktree { action: "remove", discard_changes: true }`
  rather than `git worktree remove` directly.
- If session-state gets confused, `ExitWorktree { action: "keep" }` then re-`EnterWorktree`.

---

## Per-area validation cheat sheets

### Touching `src/cmux/`

```bash
# Real-data sanity (paste this into the subagent prompt):
cmux list-workspaces --json --id-format both > /tmp/cmux-snapshot.json
cmux tree --all --json > /tmp/cmux-tree-snapshot.json
ls -la /tmp/cmux-*.json
# Inspect the actual field names before writing serde structs.
python3 -c "import json; print(sorted(json.load(open('/tmp/cmux-snapshot.json'))['workspaces'][0].keys()))"
```

### Touching `src/tui/` (any keybinding or rendering change)

1. Find the key-router in `src/main.rs` (search `is_traj_nav_key`).
2. Add new keys to the allowlist OR confirm the route-everything-in-peek/insert guard covers them.
3. Add a `TestBackend` render test if a visible change.
4. Tier 4 live launch: actually press the key in `mc` against a real workspace.

### Touching `src/mc_data/session_log.rs` or LLM regen prompt

1. Inspect real session-log files:
   ```bash
   ls -lt ~obsAgents/Sessions/*.md | head -5
   for f in $(ls -t ~obsAgents/Sessions/*.md | head -3); do
     echo "=== $(basename $f) ==="
     head -10 "$f"
   done
   ```
2. Verify `host:` and `cwd:` parsing handles real values (mixed case host;
   absolute path cwd with trailing slash; missing fields).
3. Test the resolution: pick a workspace from `cmux list-workspaces` and
   manually trace which session log `latest_session_file_for_workspace` would
   return.

### Touching `src/cli/`

```bash
cargo build --release
mc --help                              # all subcommands listed; no secrets
mc <new-subcommand> --help             # subcommand-specific help
mc <new-subcommand> <real-arg>         # end-to-end behavior
echo "exit: $?"                         # zero on success path
```

### Touching `src/llm/` (regen prompt, summarizer)

- Mock the `Summarizer` trait via the existing test pattern.
- Avoid calls to live OpenAI/Codex in tests (cost + flake).
- For prompt-shape changes: print the rendered prompt in a test and assert
  it contains the required system + user blocks.

---

## Subagent self-report contract

Every subagent dispatched to this repo must include in its final report:

1. **Status**: `DONE` / `DONE_WITH_CONCERNS` / `BLOCKED` / `NEEDS_CONTEXT`.
2. **Files modified / created**: full list, absolute paths.
3. **Test counts**: baseline → new total, formatted as `<baseline> → <new> (+N)`.
4. **Warning count**: before / after, must be ≤ baseline (10) unless a new
   warning is explicitly justified.
5. **Commit SHA(s)**: one per logical change.
6. **Validation evidence**: paste the literal `test result` summary line
   from `cargo test`; paste `cargo build`'s warning count line.
7. **End-to-end data flow trace** (F4 prevention): walk through the feature
   from user action to visible result, naming the file:line that handles
   each step.
8. **External-state inspection** (F1 prevention): for any change that touches
   an external command's output (cmux, codex, git), paste the actual real
   output of that command. Do not rely on assumed field names.
9. **Tier 4 status**: explicitly state whether the change was launched
   against live cmux. If not, say "Tier 4 deferred — manual launch needed by
   controller before merge."

Reports that omit any of items 7, 8, 9 should be treated as incomplete
regardless of green test results.

---

## Post-merge release ritual

After every merge to master:

```bash
cd /Users/blin/Tools/mission-control
cargo test -- --test-threads=1       # confirm master is still green
cargo build --release                 # update the symlinked binary
mc --help | head -10                  # smoke that subcommands still list
```

The `mc` symlink chain (`~/.cargo/bin/mc → ~/.cargo/bin/mission-control →
target/release/mission-control`) means `cargo build --release` IS the deploy
step. Skipping it leaves the user on the previous version even though
master moved.

---

## Highest fidelity rung available

- [x] Static / typecheck (`cargo check`, `cargo clippy`)
- [x] Unit (in-crate `#[cfg(test)]`)
- [x] Integration (`tests/*.rs` — single-threaded)
- [x] Real-deps E2E (`mc` launched against the user's live cmux)
- [ ] Scripted TUI snapshot diff (no harness yet — manual visual check tops out here)

For user-visible TUI changes, the gate currently caps at "binary launches,
renders, exits cleanly" plus a manual visual check. A scripted snapshot test
(e.g. `insta` + headless terminal + crossterm event injection) would raise
the top rung. Until then, **the live `mc` launch is the non-negotiable gate**.

---

## Test entry points (quick reference)

```bash
cargo test -- --test-threads=1                          # everything
cargo test --test cli_smoke -- --test-threads=1         # mc subcommands
cargo test --test cli_bind -- --test-threads=1          # mc bind
cargo test --test cli_prompts -- --test-threads=1       # promote-rules / record-hit / gc
cargo test --test mc_data_paths -- --test-threads=1
cargo test --test mc_data_trajectory -- --test-threads=1
cargo test --test mc_data_workspace -- --test-threads=1
cargo test --test mc_data_events -- --test-threads=1
cargo test --test mc_data_snapshots -- --test-threads=1
cargo test --test mc_data_inputs -- --test-threads=1
cargo test --test mc_data_prompts -- --test-threads=1
cargo test --test mc_data_session_log -- --test-threads=1
cargo test --test mc_data_session_log_mapping -- --test-threads=1
cargo test --test mc_data_user_intent -- --test-threads=1
cargo test --test peek_agent -- --test-threads=1
cargo test --test app_provision -- --test-threads=1
cargo test --test trajectory_edit_integration -- --test-threads=1
cargo test --test trajectory_watcher -- --test-threads=1    # #[ignore]-d FSEvents tests
cargo test --test tui_sidebar_render -- --test-threads=1
cargo test --test llm_regen -- --test-threads=1
cargo test --test llm_learning -- --test-threads=1
cargo test --test llm_surface_summary -- --test-threads=1
cargo test --test llm_retry -- --test-threads=1
cargo test --test dismissal -- --test-threads=1
```
