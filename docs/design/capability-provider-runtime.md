# Capability provider runtime

> Status: target design contract.
> Last updated: 2026-07-15.

This document defines Astra's protocol-independent provider, tool identity,
discovery snapshot, invocation, and typed outcome contracts. It is a detailed
sub-contract of [capability-system.md](capability-system.md).

The current implementation does not yet satisfy every requirement here. Active
implementation phases and findings are tracked outside canonical design docs.

## Ownership

This document owns:

- provider adapter and discovery snapshot boundaries;
- stable internal capability/tool identity;
- resolved descriptor and model projection separation;
- provider-claim provenance and semantic resolution;
- per-invocation policy inputs and identity;
- provider dispatch certainty and typed call outcomes;
- the boundary between invocation deduplication, semantic result caching, and
  prompt caching;
- cross-provider conformance and unhappy-path invariants.

It does not own:

- detailed permission rules, owned by
  [safety-and-permissions.md](safety-and-permissions.md);
- provider selection and fallback priority, owned by
  [capability-system.md](capability-system.md);
- session/run recovery policy, owned by
  [runtime-lifecycle.md](runtime-lifecycle.md) and
  [durable-agent-runs.md](durable-agent-runs.md);
- prompt construction and cache placement, owned by
  [prompt-lifecycle.md](prompt-lifecycle.md);
- artifact retention and debug bundles, owned by
  [artifacts-and-debug-bundles.md](artifacts-and-debug-bundles.md);
- runtime authority, owned by
  [edge-runtime-tool-boundary.md](edge-runtime-tool-boundary.md).

## Core contract

```text
provider adapter → lossless discovery snapshot
        ↓ validate, normalize, and content-address
resolved provider snapshot + internal identity/descriptor
        ├─ deterministic model/context projection
        └─ per-invocation policy resolution
                         ↓
                durable invocation ledger
                         ↓
                  provider execution
                         ↓
                  typed result envelope
```

Projection, admission, permission, scheduling, execution, retry, caching,
recording, resume, replay, and diagnostics must refer to the same resolved
descriptor and provider decision.

Static and dynamic describe how a declaration enters Astra. They do not create
different execution semantics.

## First principles

### Astra owns semantics; protocols supply evidence

External protocols, local registries, and Edge runtimes supply declarations,
claims, and call outcomes. Astra owns identity, trust, admission, policy,
durability, projection, and recovery.

A provider field may inform policy. It does not become policy merely because it
arrived on the wire.

### Identity precedes naming and projection

Internal identity is the provider binding plus native capability identity. An
exact descriptor version identifies the declaration Astra resolved.

Public tool names, model schemas, prompt order, and client labels are
deterministic projections. They are not durable execution or replay keys.

### One fact source

Visibility and execution must originate from the same resolved provider
snapshot. Independent name lists, effect maps, routing maps, and cache maps are
semantic shadow systems and are forbidden.

### Effect, idempotency, and invocation identity are orthogonal

- Effect describes which state or authority domain a call may affect.
- Idempotency describes whether replaying an operation is semantically safe.
- Invocation identity describes whether two deliveries are the same logical
  call.

Equal names and arguments do not imply equal invocation identity. A pure read
may become stale, an idempotent write may still require permission, and an
external mutation may be intentionally repeated.

### Failure is typed and scoped

Provider tool failure, rejection, transport failure, protocol failure, and
ambiguous dispatch are different outcomes. They must not be rediscovered from
human-readable strings.

A malformed tool should be isolated at the smallest safe boundary. A local
declaration error must not silently disable unrelated provider capabilities.

### Optimizations never become authority

Prompt caching, semantic read-result caching, batching, and compaction may
change latency or cost. They must not grant authority, fabricate success,
collapse distinct intent, or determine whether a session can continue.

## Plane ownership

| Plane | Owns | Must not own |
| --- | --- | --- |
| Provider adapter | Protocol/auth transport, lossless discovery decoding, native call encoding/decoding | Astra permission, retry, cache, prompt, or result policy |
| Resolution | Immutable provider snapshot, internal identity, normalized claims, schema/version validation | Per-call mutable state |
| Decision | Admission, permission, concurrency, retry, cache, route, projection policy | Provider-specific parsing |
| Execution/durability | Invocation state machine, dispatch, typed outcome, reconciliation | Model/client presentation conventions |
| Context/projection | Public aliases, model schemas, prompt-cache zones, resource/result projections | Execution truth |
| Observation | Decision/outcome lineage, cost, cache and degraded facts | Alternate control flow or silent recovery |

Dependencies flow downward. Adapters provide declarations and typed outcomes;
they do not call back into core policy. Projection consumes resolved facts and
never becomes the source used for execution.

## Provider adapters and typed facets

General infrastructure does not mean one untyped
`execute(Value) -> Value` interface. Identity, binding, snapshotting,
provenance, health, and diagnostics are shared. Tools, resources, skills,
prompts, and models retain typed facets where lifecycle and safety differ.

Conceptually:

```rust
trait ProviderAdapter {
    async fn discover(&self, binding: &ProviderBinding)
        -> Result<ProviderDiscoverySnapshot, ProviderError>;
}

trait ToolExecutionAdapter: ProviderAdapter {
    async fn execute(&self, invocation: &ProviderInvocation)
        -> Result<ProviderCallOutcome, ProviderError>;
}

trait ResourceAccessAdapter: ProviderAdapter {
    async fn list_resources(&self, query: &ResourceQuery)
        -> Result<ResourcePage, ProviderError>;
    async fn read_resource(&self, request: &ResourceReadRequest)
        -> Result<ProviderResourceOutcome, ProviderError>;
}
```

A provider may implement one or several facets. The resolved provider snapshot
composes them without erasing their types.

Adapters must:

- preserve relevant protocol declarations and typed outcomes;
- redact credentials from errors, traces, and persistent projections;
- attach source/protocol provenance to claims;
- expose capability/version changes deterministically;
- distinguish acknowledged tool failure from protocol/transport failure;
- report execution dispatch certainty when a call fails outside an
  acknowledged tool result.

Adapters must not:

- decide approval, retry, semantic caching, batching, or prompt placement;
- convert missing claims into optimistic semantics;
- turn execution errors into success-shaped output;
- silently route through a different provider;
- use a model-facing alias as native identity.

## Discovery and resolution

### Lossless discovery snapshot

```rust
struct ProviderDiscoverySnapshot {
    provider_identity: ProviderIdentity,
    binding_ref: ProviderBindingRef,
    protocol_capabilities: ProtocolCapabilities,
    tool_declarations: Vec<ProviderToolDeclaration>,
    resource_declarations: Vec<ProviderResourceDeclaration>,
    content_hash: String,
}
```

The snapshot contains decoded declarations and claim provenance, not final
Astra policy. Credentials and transport secrets are references owned by the
binding layer, never snapshot content.

Snapshot construction must:

- reject empty provider, binding, and native identities;
- canonicalize schema/object serialization;
- sort by stable internal identity, never discovery response or hash-map order;
- reject duplicate native identities within one binding;
- preserve optional versus explicitly false claims;
- derive versions/hashes from semantic content, not timestamps or process IDs;
- exclude volatile health samples and call statistics from descriptor hashes.

### Provider claims

Claims retain their origin:

```rust
struct ProviderClaim<T> {
    value: T,
    source: ProviderClaimSource,
}

struct ProviderToolClaims {
    read_only: Option<ProviderClaim<bool>>,
    destructive: Option<ProviderClaim<bool>>,
    idempotent: Option<ProviderClaim<bool>>,
    open_world: Option<ProviderClaim<bool>>,
}
```

Trust is assigned by Astra's resolver from binding authority, protocol,
deployment, policy, and source provenance. An adapter cannot declare its own
claims trusted.

Missing claims remain missing. Unknown extension values are not coerced to a
known class. Contradictory claims produce an explicit malformed or conservative
resolution according to policy, with a diagnostic attached to the affected
descriptor.

### Resolved provider snapshot

One resolver transforms discovery into a content-addressed
`ResolvedProviderSnapshot`. It owns:

- validated internal identities;
- normalized declarations and schema hashes;
- claim trust and resolution reasons;
- semantic baselines;
- deterministic model/client alias projection;
- provider/binding/route references;
- admission and degraded facts.

No provider adapter constructs final permission, retry, or cache policy.

## Tool identity and descriptor references

```rust
struct ToolIdentity {
    provider_binding: ProviderBindingRef,
    native_tool_id: String,
}

struct ResolvedToolDescriptorRef {
    identity: ToolIdentity,
    descriptor_version: String,
}

struct ResolvedToolDescriptor {
    identity: ToolIdentity,
    native_tool_name: String,
    input_schema: Value,
    output_schema: Option<Value>,
    schema_hash: String,
    provider_snapshot: ResolvedProviderSnapshotRef,
    claims: ProviderToolClaims,
    semantic_baseline: ResolvedToolSemantics,
    descriptor_version: String,
}
```

`ToolIdentity` remains stable when a public alias changes. A descriptor version
changes when the declaration or relevant resolved semantics change.

Model-facing projection is separate:

```rust
struct ModelToolProjection {
    public_name: String,
    descriptor: ResolvedToolDescriptorRef,
    model_schema: Value,
}
```

When a model emits a public tool name, Astra resolves it once through the exact
projection snapshot and records the descriptor reference in the invocation.
Permission, execution, retry, recording, resume, and replay never look the tool
up again by a bare name.

Alias collisions must not select an arbitrary route. The affected projection
is deterministically namespaced or rejected before the model can invoke it.

## Semantic baseline and invocation policy

The descriptor stores primitive claims and a resolved semantic baseline with
trust and rationale. Policy is derived again per invocation because arguments,
mode, authority, workspace state, and provider health may matter.

```rust
struct ResolvedInvocationPolicy {
    descriptor: ResolvedToolDescriptorRef,
    provider_decision: ProviderDecisionRef,
    admission: AdmissionStatus,
    route: ToolExecutionRoute,
    classification: ToolClassification,
    approval: ApprovalPolicy,
    concurrency: ConcurrencyPolicy,
    retry: RetryPolicy,
    semantic_cache: SemanticCachePolicy,
    result_projection: ResultProjectionPolicy,
}
```

Batching, permission, retry, semantic caching, execution, and recording consume
this one object. Derived booleans such as `is_read_only`, `parallelizable`, and
`cacheable` must not be independently recomputed downstream.

Conservative resolution for missing or insufficient metadata is explicit:

```text
effect             = mutating/unknown
idempotency        = non-idempotent
concurrency        = serial
approval           = required according to authority/risk policy
semantic cache     = disabled
diagnostic         = metadata_missing_or_unknown
```

This fallback keeps the capability visible when safe while refusing to invent
stronger guarantees.

## Invocation identity and duplicate control

Three mechanisms are separate:

| Mechanism | Identity | Purpose |
| --- | --- | --- |
| Invocation delivery deduplication | owner/session/run/turn/invocation ID | Prevent duplicate delivery/execution of the same logical call |
| Semantic read-result cache | descriptor + canonical args + freshness context | Optionally reuse a fresh successful pure-read result |
| Repetition/stall policy | sequence of distinct invocation IDs and observations | Detect likely model loops without collapsing user intent |

Prompt caching is a fourth, unrelated optimization owned by prompt lifecycle.
A prompt-cache key is never an invocation or semantic result-cache key.

Equal names and arguments with different invocation IDs are distinct calls.
The same invocation ID across retry/reconnect/resume returns its durable state
or terminal outcome.

Semantic read caching requires enough freshness context:

```text
provider decision
tool identity and descriptor/schema version
canonical public arguments hash
workspace/resource/provider revision or ETag
policy/context snapshot version where relevant
```

Only successful typed pure-read outcomes are eligible. Errors, timeouts,
rejections, and unknown outcomes are never cached as success.

The durable invocation decision freezes cache eligibility and the exact policy
and descriptor versions, but it does not freeze a resource revision as current
truth. Freshness facts are transient evidence and must be resolved again for
every delivery attempt, including resume of an existing `Prepared` invocation.
A restored checkpoint, a serialized cache key, or a process-local mutation
counter is not proof that the current workspace/resource still has the same
revision.

A cache hit may complete a prepared invocation only when the observation's
full key exactly matches the key built from current evidence. Matching only
tool, arguments, and policy is insufficient. A newly executed read may be
published as an observation only when one of these conditions holds:

- the provider executed a conditional read bound to the claimed revision; or
- Astra resolves the same freshness key again after the provider outcome and
  before publication.

If revalidation changes, fails, or becomes unavailable, Astra retains the
durable invocation result but abandons the cache fill. This is an observable
uncached/degraded path, not a tool failure and not a session stop.

## Durable invocation ledger

```text
Prepared
   │ durable intent recorded
   ▼
Dispatched
   ├──────────────► Succeeded
   ├──────────────► Failed
   ├──────────────► Rejected
   └──────────────► OutcomeUnknown
```

The ledger records:

- owner/session/run/turn/invocation identity;
- exact descriptor and provider decision references;
- canonical arguments hash;
- resolved effect/idempotency policy;
- provider idempotency key, if any;
- state, attempts, timestamps, result/error reference;
- dispatch certainty and reconciliation state.

Execution rules:

- persist `Prepared` before dispatch;
- resolve an existing invocation identity from hot or retained archive
  evidence independently of current run executability, so terminal outcomes
  remain deterministically replayable after closure and resume;
- serialize creation of a new identity with the run closure boundary;
- use a compare-and-set transition so concurrent workers cannot dispatch one
  invocation twice;
- revalidate run executability under the same durable closure lock and
  transaction as `Prepared -> Dispatched`; prior admission is not authority to
  cross the provider boundary after the run closes;
- persist the terminal typed result before exposing durable success;
- retry idempotent work only when the downstream contract makes it safe;
- never retry a non-idempotent call after ambiguous dispatch;
- represent a lost acknowledgement after possible external application as
  `OutcomeUnknown`;
- at an independent execution edge, persist `Prepared` before crossing the
  local executor boundary and distinguish it from `Running` during recovery:
  `Prepared` is safe to resume under the same identity, while `Running`
  becomes `OutcomeUnknown` and is never implicitly redispatched;
- keep the independent Edge inbox/outbox crash-safe with a bounded append-only
  WAL and periodic atomic snapshots. Capacity exhaustion is an explicit
  retryable `NotDispatched` admission result, not a fabricated tool outcome;
- reconcile through the provider when supported; otherwise expose uncertainty
  and require an explicit decision.

Without downstream idempotency or reconciliation, Astra cannot guarantee
strict exactly-once external effects across the remote-apply/local-ack crash
window. The product contract must state the achievable guarantee.

## Typed provider and runtime outcomes

An acknowledged provider call has a typed outcome:

```rust
enum ProviderCallOutcome {
    Success(ProviderCallPayload),
    ToolFailure(ProviderCallPayload),
    Rejected(ProviderRejection),
}
```

Provider execution errors outside an acknowledged tool result include dispatch
certainty:

```text
NotDispatched
MayHaveDispatched
```

The runtime maps these facts into its durable outcome state. It does not infer
failure from words such as `error` in output text.

The runtime result envelope preserves:

```text
invocation and descriptor/provider decision references
typed outcome and error kind
retryability and dispatch certainty
bounded visible summary and structured preview
owner-bound raw artifact reference
content hash and original size
quality and redaction status
duration and cache/replay provenance
```

Large raw results are stored once as owner-bound artifacts when policy permits.
Model, client, trace, and learning surfaces receive bounded projections derived
from the same envelope. A client event limit applies to the entire event, not a
few known fields.

## Resource context

Provider resources, request attachments, catalog files, and authoring resources
are normalized into a typed, versioned, owner-scoped resource manifest.

The complete manifest is durable and addressable. Prompt projection is bounded
by aggregate encoded-byte/token budget and includes manifest identity, version,
hash, counts, deterministic entries, `has_more`, and a list/search/read path.

This avoids both failure extremes:

- silent truncation that loses resource identity;
- unbounded request-controlled prompt growth.

Replay records the exact manifest and projected entry set the model saw.

## Prompt-cache interaction

Prompt caching is derived from deterministic context/tool projection and never
controls capability semantics.

- always-load schemas form a canonical stable prefix;
- dynamic/deferred schemas form a deterministic controlled tail;
- current resource projections, health, task state, counters, memory,
  reflection, and tool evidence remain volatile;
- real prompt, policy, authorization, descriptor, schema, or serialization
  changes invalidate the affected cache region;
- cache unsupported, miss, or write failure changes cost/latency only;
- provider fallback rebuilds the provider-native request and never reuses an
  incompatible cache identity.

See [prompt-lifecycle.md](prompt-lifecycle.md) for cache boundary and provider
serialization details.

## Failure containment

| Failure | Required behavior |
| --- | --- |
| One declaration has malformed metadata/schema | Isolate or conservatively resolve that declaration; keep unrelated valid capabilities |
| Discovery transport/auth/protocol fails | Provider degraded/unavailable with reason; never silent permanent absence |
| Claims missing or contradictory | Explicit conservative/malformed resolution with provenance |
| Tool projected but descriptor/route unavailable | Invalid invariant; block execution and emit decision mismatch |
| Provider returns typed tool error | Record failure; never reuse as successful result |
| Dispatch may have occurred | `OutcomeUnknown`; reconcile or request explicit decision |
| Resource projection exceeds budget | Durable full manifest plus bounded projection/reference |
| Artifact persistence fails for large output | Preserve typed outcome, mark raw evidence unavailable/degraded, keep event bounded |
| Client disconnects | Durable execution continues; reconnect replays durable bounded projections |
| Prompt/result cache unavailable | Continue correct uncached execution and expose cache state |

Fallback is never silent. A fallback provider requires a new observable
provider decision and must preserve the requested authority and result
contract.

## Observability

Trace, introspection, and durable audit must expose, with redaction:

```text
provider identity, binding and snapshot version/hash
internal tool identity and descriptor version/hash
public projection alias and projection snapshot
claim sources, trust and semantic resolution reason
provider decision and resolved invocation policy
invocation identity, ledger state and attempt
dispatch certainty and reconciliation state
typed outcome/error and result/artifact references
prompt/result cache state and invalidation reason
degraded, quarantine and fallback facts
```

Observation records decisions and outcomes. It must not implement alternate
fallback, retry, or cache control flow.

## Required invariants and tests

### Unit and property tests

- Equivalent discovery inputs produce byte-identical canonical snapshots and
  hashes independent of response/map order.
- Empty/duplicate native identities and invalid schemas fail deterministically.
- Public alias changes do not redefine internal identity.
- Descriptor/schema/claim changes produce a new descriptor/snapshot version.
- Optional, false, unknown, and contradictory claims remain distinguishable.
- Every claim combination resolves deterministically and conservatively.
- Result failure is determined by typed outcome, never output prose.
- Cache keys include the correct identity and freshness dimensions.
- Whole model/client projections remain within their assigned bounds for
  arbitrary UTF-8 and nested payloads.

### Adapter conformance

Standard MCP, built-in, Edge, and at least one extension-bearing fake adapter
must satisfy one suite:

- lossless relevant declaration conversion;
- deterministic snapshot construction;
- public alias collision handling;
- typed success/tool-failure preservation;
- protocol/transport error classification and dispatch certainty;
- malformed sibling isolation;
- reconnect and declaration-change behavior;
- secret redaction.

### Integration and fault injection

- Projection, admission, permission, route, execution, and recording use one
  provider decision and descriptor reference.
- A public-name call resolves once and remains bound across later discovery
  changes, resume, and replay.
- Equal arguments with distinct invocation IDs execute distinctly.
- Retry/reconnect with the same invocation ID returns one durable outcome.
- Faults before dispatch, after dispatch, after provider application, after
  acknowledgement, and before persistence produce the required ledger state.
- A malformed tool does not remove a valid sibling.
- Resource manifests and raw results remain owner-isolated and recoverable
  while prompt/client projections remain bounded.
- Prompt/result cache failures never consume an agent round or stop a session.

## Non-goals

- No protocol/provider-specific semantic shadow map in generic runtime code.
- No strict exactly-once claim without downstream idempotency/reconciliation.
- No unbounded prompt or client result surface.
- No prose-based error inference where typed facts exist.
- No silent provider fallback, cached success, context truncation, or tool
  disappearance.
- No lowest-common-denominator capability API that erases typed tool/resource
  lifecycle and safety semantics.
- No compatibility constraint from a current provider upgrade path may define
  Astra's canonical internal model.
