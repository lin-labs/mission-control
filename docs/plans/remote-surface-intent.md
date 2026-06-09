# Remote-surface intent (overall + latest ask) via screen-grab + frame merge

**Issue:** mission-control-g9x · **Status:** phase 1 landed (frame merge core)

## Problem

Each agent surface in the trajectory should show two lines:

1. **overall** — what it's trying to do overall
2. **latest** — the most recent user ask ("previous attempt right now")

For **local** agents this can come from the cmux agent-event bridge / per-surface
binding. But the **remote** box (labs) runs **tmux, not cmux** — there is no
event bridge, no hook relay, no per-surface binding reaching mc. From the local
side a remote agent is just *one cmux pane running `mosh labs`*; cmux sees a
shell, not an agent. Earlier ideas (synced vault, rsync/SSH pull, cmux events)
all either don't reach labs or depend on infra labs doesn't have.

## Approach (chosen)

When mc detects a surface is `mosh`/`ssh` to a remote, **watch the screen**:
grab the pane every ~5s, merge the overlapping captures into a deduplicated
transcript, and run change-gated LLM inference over it to extract
`{overall, latest_ask}`. No remote cooperation required — purely local
observation of what's already painted. Render identically to local surfaces.

The hard sub-problems and how they're solved:

- **Overlapping captures** — capture N+1 is usually capture N scrolled up by a
  few lines (≈900/1000 overlap). Recover the **scroll delta** and append only
  the lines that scrolled in. *Not* line-set dedup (destroys legit repeats +
  order). → `frame_merge::scroll_delta` (anchor voting).
- **"How do you know what to strip" (12s vs 12m)** — don't hardcode volatile
  patterns. **Learn** them: once two lines are *proven* aligned, whatever
  differs is the volatile span (by diff). → `frame_merge::mask_volatile`.
- **Repainting / mosh** — agent TUIs repaint and mosh syncs only the visible
  screen. Validated against real captures that anchors stay stable under it
  (100+ agree per frame); the live status line is peeled via the learned diff.
- **suggestion ≠ user ask** (easy screen-grab mistake) — three layers:
  (1) region-exclude the bottom composer/status chrome; (2) a submitted message
  persists & scrolls upward while placeholders/suggestions/in-progress typing do
  not; (3) explicit LLM guardrail that returns `null` rather than guess.

## Architecture

```
detect mosh/ssh surface
   └─ grab pane /5s (backoff when idle) ── cmux rpc surface.read_text
        └─ frame_merge: strip → region-exclude → anchor-vote delta → append new
             └─ peel live status line (learned volatile)
                  └─ change-gated LLM infer {overall, latest_ask}  (Haiku)
                       └─ cache → ~/data/mission-control/.data/<ws>/surfaces/<sid>.remote-intent.json
                            └─ render two lines (same path as local)
```

## Phases

### Phase 1 — frame merge core ✅ (landed)

`src/mc_data/frame_merge.rs`, validated against six real `mosh`→labs frames in
`tests/fixtures/remote_frames/` (`tests/mc_data_frame_merge.rs`):

- `strip_universal` — ANSI/CSI + trailing whitespace (format-independent only).
- `transcript_region` — cut composer box + tmux status bar (anchored on the `❯`
  prompt and `─` rules).
- `scroll_delta` — anchor-vote consensus; needs ≥2 agreeing anchors.
- `new_lines` — append only the scrolled-in tail.
- `mask_volatile` / `is_status_update` — learned volatile masking + live-status
  detection.
- `FrameMerger` — stateful accumulator: `ingest(raw) -> new_line_count`, holds
  the deduped `transcript` (bounded) and the current peeled `status`.

Measured on the fixtures: idle→delta 0 (0 new), scrolls→delta 18/35/4/4, 50–150
anchors agreeing each transition.

### Phase 2 — remote detection + grab loop ✅ (landed)

- `surface_kind::is_remote_comm` + `detect_remote_all` — detect remote surfaces
  by **foreground process** (`mosh-client`/`ssh`/`autossh`/…) via one shared
  `ps -A` pass (`collect_fg_last_comms`). Validated against real `mosh-client`
  foreground processes on labs ttys.
- `mc_data::remote_intent::RemoteWatch` — owns a `FrameMerger` per remote
  surface, an idle-backoff schedule (`stride`/`due`: 5s while changing, up to
  ~30s when idle), and a debug-transcript dump to
  `~/data/mission-control/.data/<ws>/surfaces/<sid>.remote-transcript`.
- TUI wiring: a 5s `remote_grab_interval` arm → `App::spawn_remote_grabs`
  (detect + capture due remote surfaces off the main loop) → `RemoteGrabUpdate`
  channel → `App::apply_remote_grab` (feed merger). Mirrors the `ScreenUpdate`
  pattern; never blocks the UI.
- `mc remote-grab-probe <surface> [--iters --interval]` — read-only CLI that
  exercises the whole loop against a live pane. Validated on `surface:29`
  (mosh→labs): 50→19→0 dedup, coherent merged transcript, idle detection works.

Not yet rendered — phase 3 turns the merged transcript into the two displayed
lines.

### Phase 3 — inference + render ✅ (landed)

- `xai::infer_intent` — xAI Grok (`grok-4-fast-non-reasoning`, `XAI_API_KEY`)
  extracts `{overall_goal, latest_ask}` from the merged transcript with the
  strict submitted-vs-suggested guardrails; returns null fields when no genuine
  user message is present rather than guessing.
- Change-gated: `RemoteWatch::transcript_for_inference` only fires after the
  transcript grows ≥8 new lines since the last inference (or first time), so the
  LLM isn't called every 5s tick.
- Cache: the inferred intent lives in `RemoteWatch` (in-memory, per surface);
  `all_intents()` snapshots it for the projection. The projection sources
  `Remote`-surface intent from this cache and renders the two lines via the same
  `format_surface_text` path as local surfaces. (Restart-persistent JSON cache is
  a phase-4 nicety, not yet implemented — intent re-derives within ~10s of a
  fresh launch.)
- Flow: `apply_remote_grab` returns the transcript to infer → main loop fires the
  xAI call off-loop → result returns via `RemoteIntentUpdate` →
  `apply_remote_intent` stores it. `mc remote-grab-probe` prints the inferred
  intent for validation.

Validated live on surface:30 (labt): `overall`/`latest` both populated in the
TUI from the screen-grab transcript.

### Phase 4 — refinements

- `mask_volatile` → token-level LCS diff for tighter multi-span status lines
  (current v1 masks a single prefix→suffix span; fine for hashing).
- Deep first-grab (`--lines N`) to recover "overall" for sessions already in
  progress when mc attaches (otherwise overall is best-effort "earliest
  observed").
- Optional exact identity via a tmux pane-title marker set on labs.

## Open questions

- **Overall recovery**: accept "earliest observed" vs one-time deep scrollback
  grab on first detection.
- **Inference scope**: every remote surface in the window vs only the selected
  one (token cost).
- **Cadence**: fixed 5s vs adaptive backoff (lean: adaptive).
