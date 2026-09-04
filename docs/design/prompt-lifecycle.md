# Prompt lifecycle

> Status: target design contract.
> Last updated: 2026-09-04.

Prompt lifecycle defines how Astra builds, versions, caches, inspects, and evolves prompts. It is distinct from context selection and tool routing, though it consumes both.

## Ownership

This document owns:

- prompt assembly phases;
- stable prefix and dynamic block boundary;
- prompt versioning;
- prompt-cache strategy;
- prompt introspection metadata;
- prompt evolution boundary.

Context selection belongs to [context-and-prompt.md](context-and-prompt.md). Provider decisions belong to [capability-system.md](capability-system.md).

## Assembly phases

```text
base contract
  -> agent profile
  -> provider/tool protocol
  -> safety policy
  -> stable examples
  -> dynamic run/session/context blocks
  -> tool schemas
  -> user turn
```

## Stable prefix

Stable prefix should contain:

- agent role and invariant behavior;
- provider decision schema;
- tool protocol;
- safety and permission contract;
- trace/introspection contract;
- stable response requirements.

It should not contain volatile provider health, long task lists, raw tool output, or sync counters.

## Dynamic blocks

Dynamic blocks should have stable keys and compact values:

```text
run_state
provider_state
task_projection
sync_state
context_summary
memory_recall
artifact_manifest
recent_trace
```

## Prompt version

A prompt version should identify:

```text
prompt_contract_version
agent_profile_version
skill_versions
safety_policy_version
provider_contract_version
tool_protocol_version
context_schema_version
```

## Prompt cache

Prompt cache goals:

- stable prefix reuse;
- minimal churn from provider state changes;
- deterministic tool ordering;
- compact dynamic state;
- no correctness dependency on cache artifacts.

ForkPrefix is a cache/diagnostic optimization, not restore correctness.

Provider cache behavior is capability-driven along independent axes: cache
protocol, physical volatile placement, optional-volatile delivery, and reuse
scope. Concrete offerings declare these facts; runtime code never infers them
from a model name. Missing legacy delivery metadata retains the pre-axis `all`
behavior, while newly serialized metadata is versioned and writes every
behavioral field explicitly.

`append_only_user_tail` is a distinct provider wire shape, not a compatibility
fallback. It is valid only with `required_only` delivery. A required runtime
control is appended as a typed, runtime-owned `role=user` frame so the next
request strictly extends the prior provider history. Provider role is not
semantic authorship: intent, memory, observer, display, turn-boundary, and
ordinary summary projections must use typed provenance and must never count
that frame as human speech. Unknown runtime provenance fails closed and cannot
be promoted to a user request.

Every append-only authority frame carries a kind and one explicit lifetime:
`next_assistant_decision` or `current_user_turn`. A later frame of the same kind
supersedes an earlier one; assistant or human-turn boundaries consume the
applicable lifetime. Retries are transactional: a failed wire assembly neither
commits a partial frame nor consumes pending authority. Cache-reusing inline
compaction may retain the exact frame bytes only under the same stable semantic
policy used by main inference; all other summary/learning projections exclude
runtime-owned frames.

When an active session changes to a non-append wire shape, expired frames are
removed from the provider projection. Active authority is unframed and re-homed
to the required system lane; kinds rebuilt by a current authoritative source
replace their historical frame instead of being duplicated. Canonical history
retains the typed record needed for deterministic resume, while the chosen
provider projection contains each effective authority kind once. Correctness is
never weakened to preserve cache reuse.

Turn focus is represented by one invariant leading-system policy: the exact
current and immediately prior text remains in canonical conversation messages
and is never recopied into the cached system prefix. Required runtime context
remains model-visible through the role required by the selected wire shape,
without changing its typed runtime ownership. For automatic-prefix protocols a
changed system message after the conversation boundary is a volatile suffix,
not a changed leading-system identity. Planned diagnostics are explicitly
labelled as a pre-client projection. Provider-final diagnostics come only from
the immutable prepared-body receipt and fingerprint the ordered message,
system, conversation, and tool-schema sequences after every provider
transformation and internal-schema sanitization. Both read the exact resolved
capability captured with the request and never guess a shape from provider or
model labels. The receipt also derives cache-key system and tool identities
from that typed capability: automatic-prefix layouts exclude post-history
system tails, while explicit-marker layouts stop at their protocol-native
system/tool marker. Every dispatched physical attempt advances this structural
baseline exactly once by durable request identity; only attempts with provider
usage contribute hit/miss counts. Pre-dispatch attempts and missing-usage
terminals never fabricate cache statistics.

Append-only canonical state uses the provider-attempt ledger as a write-ahead
boundary. Before HTTP is authorized, the same transaction that admits the
exact provider body stores every newly provider-owned canonical append as a
versioned transition with predecessor/result message counts and append-friendly
history identities. The first transition is self-contained from the canonical
coordinator's admitted durable base. Each successor stores only the lossless
gap since its parent plus the attempt-owned append; this keeps admission work
and storage proportional to new data instead of repeatedly hashing and writing
the complete growing turn. A request with no new runtime frame still stores a
recovery-only transition: provider delivery, rather than the presence of a
particular authority shape, creates the durability obligation. Initial
authority frames are authority-only appends; an internal continuation is one
atomic assistant-plus-authority append.

A resumed host uses the admitted durable message count as an ownership
boundary. It detaches the fresh request suffix, loads the database-authoritative
per-turn head, restores its bounded transition chain on the durable base, and
finally reattaches the fresh suffix. Every transition names its immutable parent
transition id and parent result identity. Provider-attempt admission validates
parent-to-current-head and advances the head in the same transaction as the
exact provider body; a same-transition physical retry is idempotent. It never
compares message values to infer lineage or guess whether repeated input such as
`continue` belongs before or after the crash. A missing parent, fork, stale
head, payload/hash conflict, or ambiguous commit without the exact head attempt
fails before HTTP or canonical mutation.
This covers a crash after HTTP authorization but before any step or canonical
checkpoint. Provider body roles, model names, prompt text, transport errors,
timestamps, run-local counters, and attempt ids are not recovery evidence.

WAL payloads cross the same durable credential-redaction boundary as runtime
checkpoints. They preserve the already-durable canonical base byte-for-byte and
redact only newly retained message data. The head tracks both chain length and
serialized bytes. Before either hard limit is reached, runtime emits a lossless
checkpoint that proves the prior head is an exact prefix, stores one
self-contained recovery anchor, and atomically retires the earlier chain. This
capacity checkpoint is not context compaction and needs no rewrite authority.
Once a canonical commit absorbs a turn, its head and payloads are retired
atomically; the next session boundary retries retirement for an earlier commit
that crashed before cleanup. Hard session deletion removes both owner-scoped
heads and attempt rows.

Prefix mismatch is not replacement authority. An append transition must prove
that the admitted durable base remains an exact prefix. A replacement
transition can be created only from the canonical rewrite proof after its
pre-mutation permit has been validated and bound to the exact resulting
predecessor identity and compaction generation. That authority is consumed when
the replacement anchor is admitted, so later provider rounds return to
incremental append until another real rewrite occurs. Otherwise provider
admission fails before HTTP.

At a text-only completion boundary, a provider may receive one request with a
stable schema declaration plus its native no-tool choice. If it nevertheless
requests a tool, the bounded repair request removes the schema declaration
physically while retaining a provider-native no-tool choice where the protocol
supports one. Cache reuse never takes precedence over terminal execution
authority.

## Prompt introspection

The system should be able to explain:

- which prompt contract was used;
- which dynamic blocks changed;
- which tools were included and why;
- which memories/artifacts were included;
- whether cache should have hit or missed.

Provider control syntax recovered from a degraded text response is runtime
protocol, not assistant prose. Streaming clients must withhold it across chunk
boundaries, while the canonical parser retains the original bytes long enough
to recover and validate the structured action.

## Evolution

Prompt changes go through tuning/evaluation gates when they affect behavior. Emergency safety prompt updates may bypass normal rollout only under explicit policy and must be auditable.
