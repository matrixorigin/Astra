# Context window management

> Status: target design contract.
> Last updated: 2026-07-07.

Context window management defines how Astra uses finite model context without losing task continuity, provider state, audit-critical facts, or prompt-cache stability.

## Principles

- Context is a scarce runtime resource.
- Recent transcript is not automatically more important than unresolved constraints.
- Provider/tool state must be compact and structured.
- Compaction must preserve recoverability and attribution.
- Large tool outputs should become artifacts or summaries, not raw prompt bloat.

## Budget zones

| Zone | Purpose |
| --- | --- |
| Stable contract | System rules, tool protocol, provider decision schema. |
| Active objective | Current user goal and constraints. |
| Runtime state | run/task/provider/sync state. |
| Relevant history | compressed transcript and unresolved decisions. |
| Retrieved memory | bounded cross-session facts. |
| Tool evidence | selected result summaries and artifact refs. |
| Reflection | strategy and uncertainty when allowed. |

## Eviction priority

Prefer evicting:

1. duplicated assistant phrasing;
2. stale intermediate tool output;
3. resolved subtask details;
4. low-confidence memory;
5. old verbose reasoning summaries;
6. raw data already stored as artifact.

Preserve:

- active objective;
- user constraints;
- pending tasks;
- blocked/degraded reasons;
- provider bindings;
- safety constraints;
- artifact references;
- decisions that affect future correctness.

## Compaction output

A compaction should produce:

```text
summary
open_questions
active_constraints
completed_work
pending_tasks
provider_state
sync_state
artifact_refs
risk_notes
```

## Prompt cache interaction

Compaction should update dynamic blocks without changing stable contract sections unless the agent contract actually changed.
