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

## The validation contract — every new development must pass this

Per-2026-05-25 directive from Boyan: every change to this repo must check
itself against this file BEFORE declaring done. The contract is:

1. Run **pre-flight** (below) at session start. Surface dirty state.
2. For each affected area, run the **per-area cheat sheet** (lower in
   this file) AND verify against the **failure modes F1–F11**. If your
   change touches any area, you cross-check the matching F-entry.
3. Run the **five-tier gate** in order. Don't skip Tier 4. The user's
   keyboard is not your CI.
4. If the change touches a NEW cmux command, NEW external command, NEW
   keybinding, or NEW per-surface behavior — open this file and look for
   the F-entry that covers it. If no F-entry exists yet and the area is
   error-prone, ADD one in the same commit. Don't wait for the next
   retro.
5. End with the **post-merge release ritual** (`cargo build --release`).

Any "DONE" report that doesn't cite the F-entries it cross-checked is
incomplete. A grep of `F[0-9]+` references in your subagent report is the
single best proxy for "did the agent actually use this file."

## Pre-flight at session start (before touching any code)

Run this once when starting work on this repo. Mid-session surprise — like
a partial 53-file merge appearing — costs at least one back-and-forth, and
in the worst case (this happened on 2026-05-24) blocks a feature build
entirely until the prior work is resolved.

```bash
cd /Users/blin/Tools/mission-control
git status --short
git diff --stat HEAD | tail -3
grep -rn "^<<<<<<<\|^>>>>>>>" src/ tests/ 2>/dev/null   # unresolved conflict markers
git log --oneline -5
```

**Required actions before writing code**:
- If `git status --short` shows more than `?? .claude/` (which is expected,
  it's the worktree dir), **surface the dirty state to the user** and
  confirm intent before staging or committing anything.
- If conflict markers exist, **stop and ask the user** how to resolve
  them. Do not unilaterally take a side — those markers represent the
  user's in-progress work on another branch.
- If `git log` shows unfamiliar recent commits (e.g. a squash-merge from a
  worktree), read the message and decide whether they impact the planned
  work before proceeding.

This pre-flight takes < 5 seconds and prevents the
"resolve conflicts mid-feature" scramble that happened in commit `cfbc3c0`
(the Mission/Goals rename squash-merge that landed while I was editing
sidebar.rs).

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

### Tier 6 — Integration back to base branch

Sprint code-complete is NOT sprint-done. After the final task subagent
reports success, the controller MUST invoke
`superpowers:finishing-a-development-branch` (or the equivalent merge-choice
prompt) before declaring the sprint complete. Boyan should never have to
ask "can you merge this to main?" — that prompt is the symptom of a missed
tier-6 gate.

**Pass criteria**: one of the 4 finishing options has been chosen and
executed:
1. Merged locally to base + branch deleted + worktree removed.
2. Pushed to remote + PR opened (worktree kept for iteration).
3. Kept as-is (Boyan explicitly opted to keep the branch open).
4. Discarded (with typed confirmation).

A controller who jumps from "T<final> complete" straight to "run validation"
or "rebuild and test" without presenting the 4 options has skipped tier 6.
Deferred tier 4 (e.g. TUI can't launch in this shell) does NOT excuse
skipping tier 6 — present the options anyway; Boyan can pick "keep as-is"
and exercise the branch first.

See AGENTS.shared.md "Sprint Completion Contract" for the cross-project
rule.

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

### F10 — cmux ref-type confusion (surface_ref vs workspace_ref)

**Symptom**: `cmux <command> returns "Workspace not found"`, or a cmux call
silently does nothing. From the user's perspective the feature doesn't
work; from the test perspective everything's green because we never
exercise the live cmux call.

**Root cause**: cmux refs come in distinct families — `window:N`,
`workspace:N`, `pane:N`, `surface:N` — and each cmux subcommand accepts
ONLY specific families. Passing a `surface:N` where the command expects
`workspace:N` doesn't fail at compile time and isn't caught by typecheck;
cmux just errors at runtime. We've burned cycles on this twice in one
session (`read-screen --workspace surface:121` → "Workspace not found";
then `select-workspace --workspace surface:121` → same error in the yield
path). Same root pattern, same blast radius, two commits apart.

**Ref-kind lookup table** (verified live against cmux on 2026-05-25; cross-check
`cmux <cmd> --help` when in doubt):

| Command                          | What goes after `--workspace` | What goes after `--surface` | Other |
|----------------------------------|--------------------------------|------------------------------|-------|
| `select-workspace`               | `workspace:N` or window UUID   | (n/a)                        | |
| `read-screen`                    | `workspace:N` or window UUID   | (n/a)                        | |
| `send`                           | `workspace:N` (required)       | `surface:N`                  | text trailing |
| `new-surface`                    | `workspace:N`                  | (n/a — emits new surface:N)  | |
| `close-surface`                  | optional context               | `surface:N` (target)         | |
| `tab-action`                     | optional context               | `surface:N` via `--tab`      | `--action <name>` |
| `focus-pane`                     | optional context               | (n/a)                        | `--pane <pane:N>` |
| `workspace-action`               | `workspace:N`                  | (n/a)                        | `--action <name>` |
| `list-pane-surfaces`             | optional `workspace:N`         | (n/a)                        | `--pane <pane:N>` |
| `move-surface`                   | optional `workspace:N`         | `surface:N` (required)       | `--pane`, `--window`, `--before`/`--after` |
| `rpc workspace.list`             | `{"window_id": "<uuid>"}` JSON | (n/a)                        | for cross-window listing |

**Prevention checklist** for any new or modified call to `cmux` in
`src/cmux/`, `src/tui/`, or `src/main.rs`:

1. Identify which ref family the cmux subcommand needs (the table above).
2. Search the call site: is the variable being passed a workspace ref or a
   surface ref? Variable names are routinely misleading — `surface_ref`
   was passed to `read-screen --workspace` for months because the type
   system can't catch a `String → String` mismatch.
3. Add a **comment** at the call site naming the expected ref kind, so
   the next reader doesn't have to re-discover.
4. If a struct field (e.g. `PeekState`) is consumed by cmux calls, name
   it after the ref-kind it carries (`workspace_ref` vs `surface_ref`),
   and document it on the field.
5. Tier 4 manual smoke: run the live cmux command from the shell first to
   confirm it accepts the ref you're going to pass. Anything that errors
   "not_found: Workspace not found" is the smoking gun.

**Grep when this F-mode is suspected** (catches every cmux call site at once):

```bash
grep -rn "client\.\(read_screen\|select_workspace\|send_text\|new_surface\|close_surface\)" src/ \
  --include='*.rs'
# For each hit, verify the first `&str` argument is the right ref-kind for that command.
```

**Cross-reference**: this F-mode was the source of bugs in commits
`2039a42`, `baba59d`, `ba20f73`, and `72ad28e` on 2026-05-24/25. Same root,
four commits, six back-and-forths with Boyan. Codifying it here as
prevention.

### F12 — Per-surface session binding not produced (no `.session-path` pointer)

**Symptom**: Two agent surfaces in the same cmux workspace peek the same
session.md (the only one whose frontmatter's `workspace_id` matches the
workspace UUID via tier-2 fallback). The user sees identical
conversation content for what should be two different agents.

**Root cause**: The peek resolver's tier-1 looks for
`<surfaces_dir>/<surface_ref>.session-path` and uses it
authoritatively. Without that producer running on agent SessionStart,
no pointer file is ever written, so the resolver falls through to
tier-2 (workspace_id), which returns one file for many surfaces.

The producer lives in the **agents repo**, not in this repo:
`~/Projects/agents/hooks/mission-control-hook.sh`. It is symlinked into
`~/.claude/hooks/` and `~/.codex/hooks/` (and reaches future agents via
the same blueprint mechanism). The hook is wired to `SessionStart` for
each supported agent.

**Prevention checklist** for any change to the peek pipeline OR the
session-binding hook:

1. **Hook is present and executable on this machine**:
   ```bash
   ls -la ~/.claude/hooks/mission-control-hook.sh
   ls -la ~/.codex/hooks/mission-control-hook.sh
   readlink ~/.claude/hooks   # should resolve into agents repo
   ```
2. **Hook's `mc bind` block exists** (not silently reverted):
   ```bash
   grep -n "mc bind --session-file" \
     ~/Projects/agents/hooks/mission-control-hook.sh
   ```
3. **`mc` resolves on PATH** (otherwise the hook silently no-ops by design):
   ```bash
   command -v mc
   ```
4. **Live verification — pointer appears for active agent surfaces**:
   ```bash
   for d in ~/data/mission-control/.data/*/surfaces/; do
     ls "$d"/*.session-path 2>/dev/null
   done
   ```
5. **Two agent surfaces in one workspace have distinct pointers**:
   Pick a workspace with ≥2 claude/codex surfaces. Peek surface A, peek
   surface B. The two peeks MUST show different content. Same content
   means tier-2 collapsed them — the hook didn't fire or didn't write
   the pointer.
6. **Smoke from a known-good cmux shell**:
   ```bash
   echo '{"hook_event_name":"SessionStart","session_id":"claude-test"}' \
     | bash ~/Projects/agents/hooks/mission-control-hook.sh
   ls ~/data/mission-control/.data/$CMUX_WORKSPACE_ID/surfaces/*.session-path
   ```
   The file should appear with the surface ref as basename (e.g.
   `surface:74.session-path`) and contain an absolute session.md path.

**Cross-reference**: producer hook lives in
`blinboyan/blin-agents` repo, file
`hooks/mission-control-hook.sh`. Consumer is the resolver step 1 at
`src/mc_data/session_log.rs::resolve_session_log_for_surface`. Producer
fix landed on agents `7e82a82` (2026-05-25); mc-side gap was F11.

### F11 — Peek source mental model: agent → session.md; non-agent → that tty

**Symptom**: peeking different surfaces in the same workspace shows the
same content (the cmux read-screen for the workspace returns a single
screen — usually whatever pane/surface is currently focused, not the
surface the user clicked). User reports "peek is pointing to the same tty
again."

**Root cause**: cmux `read-screen --workspace <ref>` reads the workspace's
current screen, not a specific surface's screen. So routing all
"non-Agent" peeks to `read-screen --workspace` collapses N surfaces onto
one stream of bytes. Separately, routing agent peeks through `read-screen`
at all is wrong: agents have a persistent transcript at
`~obsAgents/Sessions/*.md` that's the canonical source.

**The correct mental model (per user, 2026-05-25):**

- **Agent surface** (Claude / Codex / OpenCode / OtherAgent): peek the
  surface's **stored `session.md`**. If no matching session log exists,
  the peek should show an empty-with-explanation state, NOT fall back to
  cmux read-screen for the workspace.
- **Non-agent surface** (Shell / Unknown that's actually a shell): peek
  **that tty's** screen. The current cmux CLI exposes only
  `read-screen --workspace` which doesn't give per-surface granularity;
  if we need per-surface tty reads, that's a feature request to cmux OR
  we read the tty directly via the lsof+ps detection path (the same path
  used for kind detection).

**Prevention checklist** for any change to peek source resolution
(`src/tui/app.rs` peek-entry block, `src/mc_data/session_log.rs` resolver,
`src/tui/peek_view.rs::PeekSource`):

1. Agent surfaces (`SurfaceKind::is_agent() == true`) must resolve to
   `PeekSource::Agent { session_path }` or to a dedicated
   `PeekSource::AgentMissing` placeholder — NEVER to `PeekSource::Shell`.
2. Non-agent surfaces (Shell, Unknown) must resolve to a per-surface tty
   read, NEVER to a workspace-level `read-screen`. If cmux doesn't
   provide a per-surface read, prefer a clear "live tty not yet supported
   in mc-tui" placeholder over showing the wrong content.
3. Cross-surface invariant test: peek surface A, peek surface B in the
   same workspace, content MUST differ (or both must be a clear
   "unavailable" placeholder).

**Cross-reference**: this is the deeper root behind commits `2039a42`,
`baba59d`, `ba20f73`, `72ad28e`. F10 was the surface-bug (passing the
wrong ref family); F11 is the architectural bug (wrong source for the
surface kind).

### F9 — Sprint branch orphaned by skipped integration step

**Symptom**: Boyan eventually asks "let's commit all things into main"
hours-to-days after the final subagent reported success. The feature
branch has accumulated commits that aren't merged; meanwhile master has
drifted forward (parallel work + uncommitted edits). What was a clean
merge at sprint-end becomes a 13-file conflict resolution session.

**Root cause**: the controller skipped
`superpowers:subagent-driven-development`'s final integration step. The
skill's own flowchart ends with "Dispatch final code reviewer → Use
`superpowers:finishing-a-development-branch`," but the controller went
straight from "T<final> complete" to "report sprint complete + list
next-step ideas" — bypassing the merge-choice prompt entirely. The
finishing skill was even loaded into the session; it just wasn't invoked.

**Prevention checklist** for any sprint dispatched via
`subagent-driven-development` (or any multi-agent feature build on a
branch):

1. The moment the final task subagent reports DONE/DONE_WITH_CONCERNS,
   the controller's next action is `superpowers:finishing-a-development-branch`.
   Not "summarize." Not "run validation." Not "rebuild." The 4-option
   prompt comes FIRST; Boyan picks which path includes validation.
2. If a deferred Tier 4 (e.g. interactive TUI) makes the controller
   hesitate, present the 4 options anyway. Boyan can choose "keep
   as-is" and exercise the branch before deciding to merge — that's a
   conscious choice, not a default-to-orphan.
3. Cross-check: does master have uncommitted work in its working tree?
   If yes, surface that BEFORE attempting any merge. (See this session
   for the 58-file WIP that nearly got steamrolled.)

See AGENTS.shared.md "Sprint Completion Contract" for the cross-project
codification.

### F13 — LLM regen loses or reopens Mission state

**Symptom**: the Detail pane renders `## Mission` followed by `(empty)` even
though the workspace has human conversation or surface summaries, rewrites a
human-authored row, or revives a row the user already completed.

**Root cause**: prompt instructions requested Mission content, but the regen
post-processor accepted model state as authoritative. Prompt compliance alone
was treated as the invariant, and the empty-Mission scheduler did not distinguish
"never had a mission" from "all missions are completed."

**Prevention checklist** for trajectory regen changes:

1. Reconcile Mission after parsing the model response: preserve saved `[h]`
   rows byte-for-byte, preserve completed Mission history exactly, reject model
   rows similar to either set, then accept distinct agent rows.
2. Only synthesize a fallback when both active Mission and Mission history are
   empty: latest human ask first, compact session/surface summaries second, and
   a short workspace fallback last. An empty active Mission is valid when all
   work is completed.
3. Keep every synthesized fallback bullet at or below 110 characters and cap
   conversation-derived fallback at three bullets.
4. Run `cargo test --test llm_regen -- --test-threads=1`; coverage must include
   saved-human immutability, human/agent similarity suppression, exact history
   preservation, completed-work non-revival, latest-ask fallback,
   conversation-summary fallback, and the no-signal non-empty invariant.
5. Tier 4: launch global `mc`, open an actually empty active workspace, press
   `R`, wait for regen, and verify both the rendered pane and its on-disk
   `trajectory.md` contain a Mission bullet. Separately complete the only active
   Mission and verify regen does not recreate it.

### F14 — Position-based Mission history makes edits destructive

**Symptom**: `dd` deletes or shifts Mission rows unpredictably, `o` inserts into
history, or a completed Mission cannot be revived.

**Root cause**: a single vector encoded two different states by position: item
0 meant active and every later item meant history. Generic list edits therefore
changed state accidentally, and completion had no reversible operation.

**Prevention checklist** for Mission editor changes:

1. Keep active Mission rows and completed Mission history as explicit states.
   Persist them as `- [ ]` under `## Mission` and `- [x]` under
   `## Mission history`; never infer state from a row's position.
2. `x`/`X` on an active row moves it to history. Enter unfolds history, and
   `x`/`X` on a completed row moves it back to active Mission. `dd` is a no-op
   in Mission but retains ordinary deletion behavior for Beads/tasks.
3. `o` inserts below the selected active Mission as a provisional human row.
   Esc removes it without an event when empty; otherwise Esc prefixes `[h]`.
4. History is folded by default, transiently expandable, and all completed rows
   remain reachable when expanded; do not restore a fixed visible-row cap.
5. Run focused tests for completion, revival, fold/unfold, empty provisional
   cancellation, `[h]` insertion, Mission `dd` no-op, and task `dd` behavior.
6. Tier 4: in an isolated data root, exercise `o` + empty Esc, `o` + text +
   Esc, `x`, Enter to unfold, and `X` to revive. Inspect both the screen and
   `trajectory.md`; verify the pinned Release process is replaced afterward.

### F15 — Stale `.beads` overrides an authoritative Linear tracker

**Symptom**: an Olympus workspace renders `## Beads` and an unavailable row,
even though `~/agents/projects.yaml` declares the platform's tracker as Linear.

**Root cause**: task-source selection looked only for a repo-local `.beads/`
directory. A stale or redirected store therefore won over the project registry.

**Prevention checklist** for task projection and external-ticket actions:

1. Resolve workspace evidence through `~/agents/projects.yaml`. A unique
   registered feature/project identity in the workspace title or description
   wins over incidental focused cwd; feature identity is more specific than
   project identity. Match on token boundaries, accept a conservative plural
   (`group-graders` → `group-grader`), prefer the longest registered unit name,
   and do not guess when identity is ambiguous. Exact workspace/surface paths
   remain fallback evidence.
2. A declared Linear tracker stays authoritative even when its coordinates or
   credential are unavailable; do not silently fall back to `.beads/` or
   combine task rows from mismatched Linear targets.
3. Keep the persisted trajectory section canonical and change only the render
   title to `Linear`; when a feature owns the target, render a read-only
   `feature: <name>` segment row before its issues. Prove Beads workspaces still
   render `Beads` with their repo segmentation.
4. Treat projected Linear rows as read-only. Enter may open only a validated
   issue URL rooted at `https://linear.app/`; all non-ticket rows and mutation
   keys are no-ops.
5. Run focused registry, response/error, source-heading, read-only, deep-link,
   refresh-deduplication, and stale-cleanup tests.
6. Tier 4: launch global `mc` against the live Olympus workspace, verify real
   Linear issues under `## Linear`, highlight one issue, press Enter, and
   confirm the installed Linear app opens that exact identifier.

### F16 — Remote surface identity drifts or disappears while offline

**Symptom**: Mission Control shows the wrong remote agent/session, binds a
similarly named local surface, or drops a remote row as soon as its peer
disconnects.

**Prevention checklist** for arcmux mesh consumers:

1. Join only exact cmux surface UUID + workspace UUID + arcmux locator
   (`device_id`, `profile_scope`, `session_id`). Never infer identity from a
   title, cwd, session name, or newest workspace session.
2. Read only arcmux's loopback `status`, `sessions`, and `surface-bindings`
   projections. Never trigger sync per refresh, read a raw remote store, or
   connect directly to a remote host.
3. Render fresh rows normally, syncing/stale rows dimmed and retained, and gone
   rows folded out of Current. An endpoint failure retains last-known exact
   bindings as stale instead of retargeting or deleting them.
4. A bound remote peek reads its exact local cmux surface. Never fall back to a
   workspace-local transcript for a bound remote row.
5. Optional current-work text requires the exact producer provenance and must
   be single-line, bounded, and control-stripped. Skip malformed records and
   expose only bounded, sanitized warnings.
6. Run focused exact-binding, mixed local/remote, stale/reconnect,
   malformed/missing projection, workspace-isolation, and no-title-inference
   tests. Tier 4 must include a real ref/devbox disconnect and reconnect.

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
# In the pinned mc TUI: press Ctrl-R, or quit and relaunch it.
```

The `mc` symlink chain (`~/.cargo/bin/mc → ~/.cargo/bin/mission-control →
target/release/mission-control`) updates the on-disk command, but an already
running TUI keeps its old executable image. Release is complete only after the
pinned `mc` workspace has reloaded or relaunched the new binary. Before live
trajectory validation, also ensure an older parallel `mc` process is not still
refreshing the same files; it can overwrite the new projection and create a
false regression.

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
