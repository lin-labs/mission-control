# Mux State Migration

Mission-control now treats `~/data/mux/sessions/<session_id>.json` as the
authoritative activity source for mux-spawned agent sessions. The files are
read-only subscriber state written by `arcmux hook`; mission-control never
writes them.

## Inventory

| Fact collected by `mission-control-hook.sh` or prior mc status path | Mux state equivalent | Decision |
|---|---|---|
| Agent identity (`claude`, `codex`, `grok`) | `agent` | Migrated for activity display: `WorkspaceState::agent_name()` prefers mux state when mapped. |
| Working vs waiting/idle | `working`, `last_event`, `last_turn_end_at` | Migrated: `agent_state()` reads mux status first and no longer derives state from hook event names or session frontmatter `status`. |
| Last tool | `last_tool` | Migrated into the parsed reader; available for future display without reading raw hook JSONL. |
| Turn count and turn timing | `turn_count`, `last_prompt_submit_at`, `last_turn_end_at` | Migrated into the parsed reader; zero time is treated as never. |
| Raw hook event stream | `~/data/mux/hook-output/arcmux-hooks-<id>.jsonl` | Not consumed by mission-control for status; the state doc is sufficient. |
| cmux workspace id for a session | none | Kept via cmux event stream mapping. The mux session doc is per agent session and does not carry `workspace_id`. |
| cmux surface id to session-history file path | none | Kept via the optional `mc bind` SessionStart hook. This powers per-surface agent peek and is non-overlapping with mux activity facts. |
| Session history frontmatter stamping (`workspace_id`, `conversation_id`, `host`, `cwd`, `status`) | none | Kept outside status derivation. Mission-control still reads histories for trajectory/peek context, but activity status comes from mux state. |
| Device/host enable gating for the hook | none | Kept as hook operational policy; it is not an activity fact. |

## Runtime Flow

1. `cmux events --category agent` remains subscribed so mission-control can map
   `session_id` to the cmux workspace id. That mapping is not present in the
   mux session JSON.
2. On every relevant cmux event and on a short poll interval, mission-control
   reads the mapped session docs under `~/data/mux/sessions/` (falling back to
   `archived/` for ended sessions).
3. `WorkspaceState::mux_status` carries the newest mapped session state for the
   workspace. If multiple mapped sessions exist, newest `updated_at` wins.
4. `agent_state()` uses mux state first, then TypeSafe for remote panes, then
   screen regex/surface fallbacks.

Fixture tests cover the protocol schema because no live mux-spawned claude/grok
session was available during this migration.
