# Retrospective profile — mission-control

Rust TUI watching `cmux` workspaces. Retros here usually concern correctness
of cross-workspace data attribution, real-binary behavior vs unit-test
behavior, and warning budget drift.

This file is the **meta-process for retrospectives on this project**. It is
read by the `retrospective` skill at the start of every retro so the retro
knows what to look for HERE specifically. Each retro should also update this
file when it learns something durable about HOW to retro this project well.

Outcome logs go to `~obsAgents/Sessions/.../## Session Retro`. This file is
meta-process only.

Cross-reference: `.agents/validate.md` in this repo already codifies a
five-tier validation gate and a "Failure modes this repo has hit before"
(F1, F2, F3, ...) section. A good retro on mission-control work should
cross-check each F-entry against what was done this session.

---

## Project-specific signals to weight extra

- Did the change touch external command output parsing (`cmux list-workspaces
  --json`, codex CLI output, git porcelain)? If yes, verify a real-output
  fixture was used, not a guessed shape (validate.md F1).
- Did the change add a keybinding in `src/tui/`? Verify the key was added to
  `is_traj_nav_key` allowlist in `src/main.rs` (validate.md F2).
- Did `cargo build` warnings go up? Track the delta against the 10-warning
  baseline — even one new warning is a retro signal.
- Did tier 4 (live binary against real cmux) actually run, or was it
  skipped with a self-reported "compiles + tests pass"? Tier-skipping is
  the #1 root cause of post-merge breakage on this repo.
- Did the pinned `mc` process reload after the release build, and was live
  validation performed without an older parallel `mc` still rewriting shared
  trajectories? A symlink update does not replace a running executable image.
- For changes touching session-log resolution or workspace attribution: did
  the post-change manual inspection step (tier 5) get done? Cross-workspace
  contamination is silent and expensive.
- Did the session run a multi-task sprint via
  `superpowers:subagent-driven-development` on a separate branch/worktree?
  If yes, check: did the controller invoke
  `superpowers:finishing-a-development-branch` immediately after the final
  subagent's success report, OR did Boyan have to ask "let's merge to main"
  later? "Boyan-had-to-ask" is the symptom of validate.md F9 (tier-6 skip);
  the merge cost compounds with every hour of base-branch drift.
- Did the change add or modify a `cmux` CLI call? Cross-check the
  ref-kind table in validate.md F10 (workspace_ref vs surface_ref). The
  type system can't catch a `String → String` ref-family mismatch, so
  this MUST be checked by reading the call site against the table.
- For peek / per-surface UI work: did the agent vs non-agent source split
  follow validate.md F11 (agent → session.md; non-agent → that tty)?
  Crossing them is a recurring contributor to "the same content for two
  surfaces" complaints.

## Recurring failure modes (codified)

The authoritative codified list lives in `.agents/validate.md` under
"Failure modes this repo has hit before". Each retro should:

- Cross-check whether any F-entry repeated this session.
- Add a new F-entry to `validate.md` (not here) if a brand-new pattern hit
  twice.
- Use this file only for meta-patterns about HOW the retro should detect
  those failures.

Meta-patterns observed so far:

- **Same-session bug-class repetition = codify NOW**. If the SAME root
  pattern bites twice in one session (e.g. surface_ref vs workspace_ref
  in two cmux commands on 2026-05-25), open `validate.md` and add the
  F-entry in the same session. Don't wait for the next retro. Two
  occurrences is the promotion threshold.
- **Grep every callsite of a class on first detection of the class**.
  When a bug points at a *category* of API misuse (e.g., wrong cmux
  ref family), the immediate next step is a project-wide grep for all
  callers of that category, not a one-line fix on the reported case.
  This session: read-screen was fixed in `ba20f73`, select-workspace
  in `72ad28e`. Both should have landed in one commit if I'd grepped
  `client.\(read_screen\|select_workspace\|send_text\)` at first
  detection.
- **Take the user's mental model as ground truth**. When Boyan said
  "agent surface should point to the stored session.md, non-agent
  surface should point to the tty console itself" — that's the spec.
  Don't bottom-up re-derive it from code. Confirm, then implement.

## Successful patterns worth reinforcing

- Pasting 30+ lines of real external-command output into the subagent
  prompt before touching parsing code.
- Running tier 4 (`mc` against real cmux) before claiming done, even when
  unit tests are green.
- Recording warning count in PR description so drift is visible.
- **Isolated cloud-auth failure smokes**. For optional credentials, point the
  process at a fresh temporary auth config (for example `CLOUDSDK_CONFIG`) and
  live-launch the TUI. This verifies the non-fatal warning path without
  disturbing the user's real authenticated profile.
- **File-logged diagnostics for TUI bugs**. The TUI captures stderr to
  the alt-screen, so `eprintln!` is invisible to the user. Writing to
  `/tmp/mc-peek-debug.log` (or similar) with `OpenOptions::append`
  inside hot paths cracked the peek bug in one round after weeks of
  guesswork. Pattern to repeat: when a TUI bug resists 2+ analytical
  rounds, ship a file-logged diagnostic build immediately rather than
  speculate further.
- **Asking Boyan for concrete repro IDs** (e.g.
  `workspace_id=ED00E698... surface_ref=surface:121`) the moment a UI
  bug stalls. Three lines of IDs > three back-and-forths of "try this."
- **Use Mission Control's persisted window snapshots for attribution work**.
  When the live `cmux` socket is unavailable, inspect
  `~/data/mission-control/windows/*/window.json` and check its `updated_at`
  before dropping to lower-level pane inspection. The snapshot already carries
  the workspace-to-surface mapping plus each structured surface's overall goal
  and latest ask.
- **Fault the mesh, not the supervised session, for recovery dogfood**. A
  mesh-only hot reload to an unreachable peer kept the remote agent and exact
  surface alive, proved stale → fresh on the same locator, and avoided turning
  a reconnect test into daemon/session teardown. Pair it with exact peek and
  yield before cleaning the disposable binding.

## Where retro findings from this project should land

1. Meta-process about retroing this repo → this file.
2. New codified failure mode → `.agents/validate.md`'s "Failure modes"
   section (not here).
3. Cross-project rule (e.g. "always paste real output before parsing it")
   → `AGENTS.shared.md` only if it generalizes beyond this repo.
4. Reusable Rust/TUI workflow → a skill under `skills/`.

Default to `.agents/validate.md` (most lessons here are concrete and
project-specific) or this file (when the lesson is about retro process).

## Project-specific retro checklist

- For each `F<N>` in `validate.md`: did this session's work risk
  re-triggering it, and was the corresponding prevention check applied?
- Did the warning budget hold?
- Was tier 4 actually exercised on a live cmux, or only TestBackend?
- For changes to regen / peek / dismissal: was the tier-5 cross-workspace
  manual inspection done?
- Are there any "I'll add a test later" promises hanging? They're not "done"
  until the test exists.

## How to fill this in

This profile is already opinionated because mission-control has a rich
validate.md to draw from. Each subsequent retro should:

1. Read this file AND `.agents/validate.md`.
2. Conduct the retro weighting the signals here.
3. Promote new codified failure modes to `validate.md` (not here).
4. Promote new meta-process insights to this file (small — 2-5 lines).
