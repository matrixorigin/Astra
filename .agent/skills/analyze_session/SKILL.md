---
name: analyze-session
description: "Analyze local Astra session journals using `astra journal digest` plus optional checkpoints/debug dumps. Use for stalls, tool failures, compaction, token/context pressure, guard escalation, and error cascades."
user_invocable: true
when_to_use: "When the user wants to analyze a past or current Astra session for stalls, slow turns, looping, wrong tool selection, token waste, compaction, turn errors, or failed tool calls."
arguments:
  - name: TARGET
    description: "Session ID, debug JSON path (/tmp/debug-*.json), or keyword ('this', 'last'). Omit for most recent."
    required: false
  - name: FOCUS
    description: "Focus: context, tools, tokens, errors, flow, debug, or all. Default: all."
    required: false
allowed_tools:
  - bash
  - read_file
  - grep
  - glob
---

# Analyze Session

Use machine-generated evidence first. `astra journal digest` is the primary source;
raw JSONL parsing is a fallback only when digest is unavailable or missing a field.

## Task

$ARGUMENTS

## Phase 1: Resolve Target

Use the most recent local session unless a target is provided.

```bash
command -v astra
astra journal digest last --format json
astra journal digest <SESSION_ID> --format json
astra journal digest <SESSION_ID> --focus summary --format json
```

If `astra` is not on `PATH`, try an existing local binary such as
`target/debug/astra` or `target/release/astra`.

For `/tmp/debug-*.json` input, skip digest metrics and use the debug dump only for
the message/tool/prompt snapshot it contains.

## Phase 2: Trust The Digest Schema

Stable schema: `schema_version = "astra-journal-digest-v1"` from
`crates/astra-cli/src/cli/journal_digest.rs`.

Use these fields directly. Do not invent numbers.

| Field                             | Use                                                                                                          |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `aggregates`                      | Turn count, tokens, duration, tool counts, failures, stalls, compactions                                     |
| `turns[]`                         | Per-turn tokens, latency, TTFT, context time, visible/used/activated tools, selected skills, budget pressure |
| `failed_tool_calls[]`             | Failed call category, tool name, args preview, error preview                                                 |
| `compaction_events[]`             | When context was compacted and what signal triggered it                                                      |
| `stalls[]`                        | Stall/circuit-breaker evidence                                                                               |
| `turn_errors[]`, `other_errors[]` | Error cascade and failure boundaries                                                                         |
| `journal_lines_malformed`         | Whether digest skipped corrupted journal lines                                                               |

If `schema_version` differs, report the mismatch and still use fields that exist.

## Phase 3: Diagnose By Focus

Context/tokens:

- Compare `tokens_in`, `tokens_out`, `budget_pressure`, `visible_tools_count`, and compaction timing.
- A high input-token turn with low tool progress usually points to history, tool result, or skill injection bloat.
- Repeated high `budget_pressure` after compaction points to prompt assembly or tool-result retention.

Tools:

- Start with `failed_tool_calls[]`, grouped by `tool` and `error_category`.
- Compare `visible_tools_count`, `tools_used_count`, and `activated_tools_count`.
- A visible-but-unused tool is not automatically bad; repeated activation without successful use is the signal.

Stalls/looping:

- Use `stalls[]` and consecutive turns with similar `user_input_preview`, failed tools, or no new successful tools.
- Check whether the agent changed approach after a nudge or repeated the same call pattern.

Errors:

- Anchor every root cause to the first failed turn or failed tool call that made later work invalid.
- Separate permission/safety guard blocks from tool implementation failures.

Flow:

- Reconstruct the session as `user intent -> turn sequence -> tool outcomes -> compaction/stall/errors -> final state`.
- Prefer the smallest explanation that accounts for the observed sequence.

## Phase 4: Optional Deep Evidence

Use only when the digest does not answer the question.

| Evidence                       | Path                                                                                |
| ------------------------------ | ----------------------------------------------------------------------------------- |
| Heavy prompt checkpoint        | `~/.astra/sessions/<id>/step_checkpoints/*-heavy.json`                              |
| Debug full turn dump           | `/tmp/debug-*-turn*-full.json`                                                      |
| Local journal                  | `~/.astra/sessions/<id>.jsonl`                                                      |
| Session journal implementation | `crates/services/src/session_journal.rs`                                       |
| Stall/guard implementation     | `crates/runtime/src/turn/`                                                     |
| Tool surface implementation    | `crates/runtime/src/tool_registry/`, `crates/runtime/src/capabilities.rs` |

## Output Contract

```text
Findings:
- <highest-impact diagnosis with digest evidence>

Evidence:
- session=<id>, schema=<schema>, turns=<n>, failed_tools=<n>, stalls=<n>, compactions=<n>
- <turn/tool/error citations>

Root cause:
- <one concrete mechanism>

Recommended fix:
- <code owner or workflow fix>

Unknowns:
- <only if evidence is missing>
```

```skill-diagnosis
{
  "schema_version": 2,
  "skill": "analyze_session",
  "cause": "session_stalls",
  "headline": "agent stalled on repeated tool calls with no new progress",
  "findings": ["turn 4-7 repeated identical grep with no new matches"],
  "recommended_action": "narrow scope to src/ or switch to rg",
  "success_criteria": [
    {
      "metric": "session_stalls_delta",
      "operator": "lte",
      "threshold": 0.0,
      "window_turns": 3,
      "description": "session stalls stop increasing"
    }
  ],
  "source": "real_skill"
}
```
