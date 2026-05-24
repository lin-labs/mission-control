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
- For changes touching session-log resolution or workspace attribution: did
  the post-change manual inspection step (tier 5) get done? Cross-workspace
  contamination is silent and expensive.

## Recurring failure modes (codified)

The authoritative codified list lives in `.agents/validate.md` under
"Failure modes this repo has hit before". Each retro should:

- Cross-check whether any F-entry repeated this session.
- Add a new F-entry to `validate.md` (not here) if a brand-new pattern hit
  twice.
- Use this file only for meta-patterns about HOW the retro should detect
  those failures.

Meta-patterns observed so far:

(none yet — promote from "Project-specific signals" once a pattern repeats
at the retro level, not the code level)

## Successful patterns worth reinforcing

- Pasting 30+ lines of real external-command output into the subagent
  prompt before touching parsing code.
- Running tier 4 (`mc` against real cmux) before claiming done, even when
  unit tests are green.
- Recording warning count in PR description so drift is visible.

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
