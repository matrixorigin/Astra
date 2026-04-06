---
name: analyze-session
description: "Developer skill: diagnostic analysis of an astra session. Primary input is `astra journal digest` (stable JSON from local ~/.astra/sessions). Optional deep dive: heavy checkpoints, debug JSON."
user_invocable: true
when_to_use: "When the user wants to analyze a past session for token waste, tool selection accuracy, or context efficiency"
arguments:
  - name: TARGET
    description: "Session ID, debug JSON path (/tmp/debug-*.json), or keyword ('this', 'last'). Omit to analyze most recent."
    required: false
  - name: FOCUS
    description: "Interpretation focus: 'context', 'tools', 'tokens', 'errors', 'flow', or 'all' (default: all)"
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---
# Analyze Astra Session

Use **machine-generated metrics first** (`astra journal digest`), then interpret. Do not re-parse raw JSONL or recompute aggregates by hand unless digest is unavailable.

## Task

$ARGUMENTS

---

## Phase 1: Load digest (required)

Run from the project or any directory (command is offline; no login required). If `astra` is not on `PATH`, use the full path to the CLI binary (ask the user or use `command -v astra`).

```bash
# Most recent local session
astra journal digest

# Specific session (full UUID, unique prefix, or last)
astra journal digest <SESSION_ID>
astra journal digest --session <SESSION_ID>
astra journal digest last

# Smaller JSON (metrics-focused turn rows)
astra journal digest --focus summary

# Human-readable
astra journal digest --format text
```

**Stable schema**: root field `schema_version` is `astra-journal-digest-v1`. If it differs, describe the mismatch and still use whatever fields exist.

### JSON fields to use (do not invent numbers)

- **`journal_lines_non_empty`**, **`journal_lines_malformed`**: raw JSONL line counts; non-zero `malformed` means some lines were skipped during parse.
- **`aggregates`**: `session_start_count`, `session_end_count`, `turn_count`, `turn_error_count`, `compact_count`, `stall_count`, `error_event_count`, `total_tokens_in`, `total_tokens_out`, `total_duration_ms`, `total_tool_calls`, `tool_calls_failed`, `avg_tokens_in`, `avg_tokens_out`, `avg_duration_ms`.
- **`turns`**: per-turn `seq` (1-based chronological index), `turn_id` (session turn counter when present), `tokens_in` / `tokens_out`, `duration_ms`, `ttft_ms`, `context_ms`, `selector_ms`, `selector_strategy`, `tools_selected_count`, `tools_used_count`, `selected_skills`, `tool_calls_ok`, `tool_calls_fail`, `user_input_preview`, `budget_pressure`.
- **`compaction_events`**, **`stalls`**, **`turn_errors`**, **`other_errors`**: structured side events; cite `ts`, `turn`, and `detail` from JSON.

### Resolve TARGET when not using default session

| TARGET | Command |
|--------|---------|
| Omitted / `last` / `previous` | `astra journal digest` |
| UUID or short ID | `astra journal digest <id>` |
| `this` / `current` | Use active session id from user context; same as above with that id |
| Path `/tmp/debug-*.json` | Skip digest for metrics; go to Phase 3 only |

---

## Phase 2: Interpretation (by FOCUS)

Use **only** digest fields. Quote or paraphrase numbers from JSON; do not estimate token totals from prose.

**all**: Short narrative covering aggregates, token trend across `turns[].seq` (e.g. monotonic growth vs drops after `compaction_events`), tool health (`tool_calls_fail` vs `total_tool_calls`), stalls/errors.

**tokens**: Emphasize `avg_*`, per-turn `tokens_in`, and relation to `compact_count`.

**tools**: Compare `tools_selected_count` vs `tools_used_count`; `tool_calls_ok` / `tool_calls_fail`; `selector_strategy` / `selector_ms` when present.

**errors**: `turn_errors`, `other_errors`, `stalls`, `turn_error_count`; tie to neighboring `turns` by `turn` / `seq` when possible.

**flow**: User-visible cadence via `user_input_preview`, durations, and whether compactions cluster.

**context**: `context_ms`, `memoria_ms`, `ttft_ms`, `budget_pressure` patterns.

---

## Phase 3: Optional deep dive (only if user needs message-level proof)

- **Heavy checkpoints**: `~/.astra/sessions/<SESSION_ID>/step_checkpoints/*-heavy.json` — OpenAI-style messages the model saw.
- **Debug export**: If TARGET is a debug JSON file, read `schema` / `journal_turn_summary` when present.

Do not rebuild Phase-2 statistics from these files if digest already covered them.

---

## Phase 4: Report template

Keep the report compact and grounded in digest JSON.

1. **Session**: `session_id` from digest; note `journal_file` path.
2. **Headline metrics**: copy key fields from `aggregates`.
3. **Notable turns**: at most 3 entries, cite `seq` and `turn_id`.
4. **Issues**: bullet list tied to digest evidence (stalls, errors, token spikes, missing compactions).
5. **Recommendations**: 3–5 actionable items.

---

## Reference: implementation pointers

| Component | File |
|-----------|------|
| Journal digest CLI | `rust/crates/astra-cli/src/cli/journal_digest.rs` |
| Session journal | `rust/crates/services/src/session_journal.rs` |
| Tool selection | `rust/crates/runtime/src/turn/tool_selection.rs` |
| Compaction (cloud/runtime) | `rust/crates/runtime/src/turn/cloud/compaction.rs` |
| Stall detection | `rust/crates/runtime/src/stall_detector.rs` |
| REPL turn handler | `rust/crates/astra-cli/src/cli/repl_turn.rs` |
