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

Exact-output deduplication is distinct from deciding that an observation is
obsolete. A repeated path, tool name or read argument does not prove equal
content: files can change, ranges can differ and providers can change ownership.
The shared compaction layer may reference a later result only when the complete
call name, arguments, plain-text output and result metadata match. It retains
each call/result pair and references the retained result by call ID; it never
merges executions or upgrades their outcome. Ambiguous IDs, structured/array
payloads and already-compacted output are not candidates. A reference must save
both bytes and estimated tokens. Budget truncation does not infer tool semantics
from names or output prose. The compression trace reports this as
`DuplicateToolOutputElimination`; it is not a semantic read-equivalence claim.

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

Successful completion is not a compaction boundary. Canonical terminal commits
retain complete tool call/result groups, including selected deferred contracts,
just as interrupted turns do. New turns restore this canonical history, not the
display-only user/answer pairs. The shared context optimizer owns pressure-based
eviction; a verified compaction rewrite is the only authority to replace an
admitted prefix. A resumed assistant/tool suffix may continue the existing user
turn without copying or rewriting its immutable prefix. Checkpoints restore
runtime metadata, but do not substitute for the canonical commit in regression
tests of cross-turn continuity.

The latest human request remains in canonical history even after many tool
rounds. Runtime focus guidance contains a bounded rule, not copied current/prior
requests or a second `active_goal` representation. This keeps constraints intact
without multiplying large inputs after the budget pass; telemetry owns turn and
round identity independently of the prompt's focus instruction.
