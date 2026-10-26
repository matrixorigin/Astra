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

Context assembly consumes selected skills, retrieved memory, working memory,
and runtime feedback through their existing owners. The unused emergent-context
prototype is retired, including its extra skill/memory/summary/attachment
container, snapshot projection, opportunistic sections, and Explain source
kinds. Current snapshots retain warm statistics, latches, recovery, working
memory, provider-cache observations, and the session date. Older pipeline
snapshots containing retired section kinds are outside the current snapshot
layout: the existing corrupt-snapshot diagnostic initializes advisory pipeline
state afresh. Canonical journal recovery remains unchanged; no legacy emergent
restoration or compatibility mapping is provided.

Pipeline warm-start uses the complete session snapshot and its canonical
restore entrypoint. Unused partial-state constructors, standalone stats codec
wrappers, and the unconnected metrics DTO are retired. Actual statistics serde
and journal-derived session facts remain with their existing owners.

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
dynamic context message after the conversation boundary is a volatile suffix,
not a changed leading-system identity. OpenAI-compatible requests consolidate
policy into one leading system and project runtime facts as marked user
context, as specified in [context-and-prompt.md](context-and-prompt.md).
Planned diagnostics are explicitly
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

Planned prompt diagnostics identify owner, session, optional execution run,
turn, round, attempt, and source. A missing run means a session-only diagnostic;
it is not a substitute for a known run. Parent and child requests with identical
coordinates retain separate attribution. Capture and persistence consume the
same plan, and persistence rejects mismatched identity before acquiring a
connection. Content hashes remain independent of execution identity: delta
ancestry may reuse exact content across runs within the same owner/session/source.
Diagnostic rows remain opt-in and are not a count of physical provider calls.

Provider admission stores the exact request hash and byte count under the
existing invocation and attempt owners. Run generation, owner lease and
unsettled-delivery checks fence dispatch. An uncertain admission commit can
authorize delivery only after the original call verifies its exact admission
token and immutable request identity; ordinary re-entry grants no delivery.

Settled continuation restores from the existing Run checkpoint and its admitted
canonical base. Its bounded continuation payload preserves exact message
identity and crosses the checkpoint credential-redaction boundary. Recovery of
an in-flight parent transition is unsupported and requires a new turn; the
provider-attempt ledger alone cannot reconstruct uncheckpointed conversation
content or authorize resending an uncertain request. There is no separate
database canonical-head/WAL recovery protocol.

Prefix mismatch is not replacement authority. Local history replacement and
terminal Context publication require the existing typed rewrite proof bound
to the admitted base and resulting compaction generation.

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
