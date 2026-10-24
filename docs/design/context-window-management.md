# Context window management

> Status: target design contract.
> Last updated: 2026-10-07.

Context window management defines how Astra uses finite model context without losing task continuity, provider state, audit-critical facts, or prompt-cache stability.

Unused standalone request and compaction circuit-breaker APIs are retired.
They were not connected to provider forwarding or compaction execution. Active
loop limits and Memoria failure handling remain with their production owners;
this retirement does not change their admission or recovery behavior.

## Principles

- Context is a scarce runtime resource.
- Recent transcript is not automatically more important than unresolved constraints.
- Provider/tool state must be compact and structured.
- Compaction must preserve recoverability and attribution.
- Large tool outputs should become artifacts or summaries, not raw prompt bloat.

## Mechanical rewrite ownership

Canonical history stays in its original JSON representation. One mechanical
owner implements exact-output deduplication, structured text truncation and
provider-valid history pruning. The pre-turn/resume and provider-retry engine
uses its fixed progressive schedule, age/current-round protection and net token
estimates; request assembly and Memoria retain their serialized-character
budgets, latest-evidence preference and separately owned memory/summary policy.
These policies share the rewrite operations, not another message codec or an
extensible layer execution framework.

A rewrite prepares one isolated candidate and installs it only after useful
progress. Callers do not clone the result back into a temporary and then return
another copy. The canonical commit, current retry limits, session-lifetime
futile-compaction breaker and durable artifact-write-before-install ordering
remain with their existing owners. Estimated savings include new boundary and
metadata costs; they are not provider-reported token usage.

Active runtime authority remains attached to its real human anchor and ordered
current-turn suffix. Provider user-role tool-result envelopes do not start a new
human turn or expire authority. Expired authority can be pruned. If required
context alone exceeds the window, the result remains truthfully over budget;
compaction cannot delete controls to manufacture progress. Tool-call identities,
opaque extension fields, ordered non-text blocks and artifact recovery handles
survive every mechanical path. A no-op cannot silently normalize these fields.

Typed duplicate-output dependencies survive canonical commit and continuation
restore. The shared validator checks unique forward invocation identity and
original result attribution before the canonical projection removes incidental
metadata; malformed or mismatched links cannot acquire authority through that
projection. Provider requests still strip this internal bookkeeping.

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

Accepted history summaries carry a versioned `_astra_history_summary` marker
through canonical replacement and continuation restore. They retain their
existing system storage role, but the shared summary input projection passes
them as historical evidence: assistant context for inline summaries and an
explicit prior-summary section for rendered summaries. Inline requests precede
that assistant evidence with an auxiliary user task preamble, preserving the
user-first conversation shape required by providers such as Bedrock Nova,
including after resume and PTL retries. The preamble is never persisted as
human conversation history. Stable instructions and
runtime-owned controls never qualify as historical summaries. Resume also
recognizes the exact pre-marker compaction envelopes already persisted by Astra.
Both summary paths retain this evidence across PTL retries without applying
ordinary message excerpt caps. Artifact spills also carry forward prior summary
evidence and recovery handles; the existing net token reduction check still
decides whether the candidate can replace history. This is an input continuity
contract, not a guarantee of a model's semantic fidelity.

Successful request-time summaries also become the loop's canonical working set
when the summary plus bounded history saves estimated tokens. The existing
rewrite proof and existing Run checkpoint or terminal Context commit own this
transition; no separate
summary cache, memory write or checkpoint table is involved. Later pipeline and
pre-turn pressure measurements therefore see the accepted representation plus
newly appended history, rather than repeatedly summarizing the original input.
An unchanged or still-fitting working set performs no further summary inference;
new history that requires actual compaction can produce the next generation.
Summary space is reserved only when history exceeds the concrete budget after
memory costs; reserving a hypothetical next summary cannot create that pressure.

Acceptance requires a valid admitted canonical rewrite permit and input matching
either the exact canonical source or the shared pipeline's validated
provider-placement projection. If that projection re-homed append-only
authority, its active human-anchored suffix must survive unchanged and is restored
in canonical history. A stale/unrecognized projection, invalid tool pairing,
non-reducing candidate, failed summary or cancelled in-flight operation cannot
install a replacement. Request-local mechanical fallback and final wire budget
enforcement remain available. On the first canonical turn without an admitted
rewrite proof, a successful summary stays request-local and preserves the
existing provider WAL prefix. Prior summary generations are consumed as evidence
and replaced by the accepted summary, not accumulated. Resume uses the ordinary
canonical continuation path and retains that summary without another inference.

Exact-output deduplication is distinct from deciding that an observation is
obsolete. A repeated path, tool name or read argument does not prove equal
content: files can change, ranges can differ and providers can change ownership.
The shared compaction layer may reference a later result only when the complete
call name, arguments, plain-text output and result metadata match. It retains
each call/result pair and references the retained result by call ID; it never
merges executions or upgrades their outcome. Ambiguous IDs, structured/array
payloads and already-compacted output are not candidates. A reference must save
both bytes and estimated tokens. Budget truncation does not infer tool semantics
from names or output prose. The mechanical pipeline records this as `duplicate_tool_output_elimination`
in its existing outcome and compaction event. Savings are estimates; this is
not a semantic read-equivalence claim.

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

These are outputs of actual context rewriting, not a generic completion message
written after a database scan. Per-call manifests can share `post_compaction`
across several model calls; neither their count nor a scan of all session state
certifies a rewrite. Verify preservation at the canonical rewrite boundary.

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

## Context snapshot evidence

`ContextAssemblyTrace.history` describes observed CLI admission input before
Server-side request assembly. `null` means unobserved; an object with an empty
`turns_retained` list and `total_tokens: 0` means observed empty input. Group
ordinals are local input ordinals, not canonical session-turn identities.
`total_tokens` is an estimate of that observed slice.

This snapshot supplies no pre/post compaction ratio, affected-turn list or
pipeline-stage history. Runtime compaction events, step facts and audit remain
their owners. A positive compression-trigger flag is evidence of a trigger,
not a reconstruction of the rewrite. Session marker lists identify turns with
recorded markers; they do not count all compactions or prove absence.
