# Observation plane

> Status: target design contract.
> Last updated: 2026-07-07.

The observation plane owns trace, audit, introspect, reflect, status, diagnostics, and supportability. It describes what the system knows about itself and what the agent/user may inspect.

This is a normative design contract, not an implementation status report.

## Principles

- Trace is structured runtime fact, not log text.
- Audit is durable accountability, not debug noise.
- Introspect reports current state and capability boundaries.
- Reflect reasons about strategy and quality within policy.
- Diagnostics must be specific enough to support recovery.

## Fact layers

| Layer | Examples |
| --- | --- |
| C0 control | session, run, task, checkpoint, leases. |
| C1 transcript | user-visible messages. |
| C2 audit | run events, permission decisions, provider decisions. |
| C3 trace | LLM rounds, tool lifecycle, retry/cache/sync decisions. |
| C4 debug | raw captures, support bundles, manifests. |
| C5 learning | redacted examples, labels, eval artifacts. |

## Default C3 events

The default trace schema should include:

- `llm_round_completed`;
- `tool_call_started`;
- `tool_call_completed`;
- `tool_call_failed`;
- `provider_decision`;
- `step_verdict`;
- `retry_decision`;
- `cache_decision`;
- `sync_status`.

Common causal fields:

```text
session_id
run_id
parent_run_id
turn_id
round_index
tool_call_id
provider_id
capability
cause_event_id
```

## Introspect

Introspect should answer:

- what state am I in;
- what providers are available;
- what tools are visible and why;
- what is blocked and how to unblock it;
- what context was loaded;
- what sync state is safe or degraded;
- what recent failures matter.

## Reflect

Reflect is agent reasoning over observation facts. It should not mutate state by itself. It can propose strategy, identify uncertainty, and request action.

## Debug bundles

Raw debug bundles are C4 and off by default.

Requirements:

- explicit user or policy enablement;
- short TTL;
- manifest;
- redaction boundary;
- export and delete operations;
- audit event for creation/access/deletion;
- exclusion from default learning pipeline.

## Diagnostics quality

Bad diagnostic examples:

- unknown tool reported as missing runtime binding;
- provider offline reported as malformed call;
- plan-mode policy denial reported as tool absence;
- sync poison hidden as generic failure.

Good diagnostics include cause, scope, affected capability, resumability, and next action.

## Trace payload contracts

### `provider_decision`

```text
event_type = provider_decision
session_id
run_id
turn_id
capability
tool_name
provider_type
provider_id
route
admission_status
runtime_binding_status
fallback_policy
fallback_from
degraded_reason
offline_reason
```

### `retry_decision`

```text
event_type = retry_decision
session_id
run_id
turn_id
round_index
tool_call_id
retry_reason
retryable
attempt
max_attempts
next_action
```

### `cache_decision`

```text
event_type = cache_decision
session_id
run_id
turn_id
prompt_contract_version
stable_prefix_hash
dynamic_block_hash
cache_expected
cache_hit
miss_reason
```

### `step_verdict`

```text
event_type = step_verdict
session_id
run_id
turn_id
step_id
verdict
confidence
reasons
next_action
```

### `tool_call_started`

```text
event_type = tool_call_started
session_id
run_id
turn_id
round_index
tool_call_id
tool_name
provider_id
route
arguments_hash
started_at
```

### `tool_call_completed`

```text
event_type = tool_call_completed
session_id
run_id
turn_id
round_index
tool_call_id
tool_name
provider_id
status
duration_ms
quality_status
result_artifact_ref
completed_at
```

### `tool_call_failed`

```text
event_type = tool_call_failed
session_id
run_id
turn_id
round_index
tool_call_id
tool_name
provider_id
error_kind
retryable
quality_status
fallback_available
failed_at
```

## Model request attribution and usage

The inference ledger stores content-free accepted/terminal request diagnostics
in `model_request_context_events`. These records complement `agent_events` and
are exposed through the existing owner-scoped request queries and session
observability projection. They do not require full prompt capture.

`ModelRequestContextEvent.route` projects non-secret facts from the admitted
inference plan:

```text
route_id
invocation_id
upstream_model
execution_placement
access_kind
```

The adjacent request identity owns the Offering, configured model, purpose,
session/run/turn/round, and physical request ID. Accepted and terminal events
must agree on route attribution, including recovery after process failure.
Older records without this projection deserialize with an unknown route;
readers must not infer selection policy from a model name or missing field.
Credentials, owner/admission tokens, endpoint URLs, and raw prompt content do
not belong in this projection.

Request diagnostics use strict typed readers. Deployments adding route fields
must upgrade readers and recovery workers before new producers, or drain old
instances during a coordinated upgrade. Backward reading of stored records
without route fields does not imply older binaries can read the new projection.

Usage coverage is independent of request outcome:

- `provider_exact`: observed usage is available, including a measured zero.
  Full-input budget errors and cache-read share may be computed.
- `provider_partial`: retain observed token lanes, but do not report them as
  a complete input measurement, budget estimate error, or cache-read share.
- `unavailable`: usage and measured diagnostic fields are absent. Placeholder
  zeroes in legacy records are not evidence of zero-token billing.

Accepted events have no provider measurements. Terminal diagnostics with
unavailable usage store nullable token columns as `NULL`. Aggregate request
counters still count those attempts; token sums contain only observed usage
and do not establish complete billing coverage. Consumers estimating cost or
building learning examples must retain per-request coverage and treat missing
or expired diagnostics as unknown. Foreground settlement and recovery share
the same coverage projection.

## Semantic judgment trace

Server lifecycle observability flushes prepare one annotated journal batch for
local persistence and the existing bounded ingestion sender. Only generic
`TraceSpan` records use this handoff; other event families retain their existing
durable owners. Trusted owner/session and the captured execution generation bind
the process-local sink once; this is not per-fact generation verification.
Generic trace IDs need not be run IDs. Typed semantic readers validate their
own run correlation. Historical facts may flush after waiting, cancellation or owner
transfer without granting the old execution new authority. Local IO failure
does not suppress enqueue or consume the retained buffer; missing/closed/full
ingestion does not suppress local persistence. Exact batch replay retains the
content-addressed storage ID. Changed interruption/eviction annotations change
that ID; semantic readers reconcile equivalent observation identities, while
generic trace readers must not assume annotation-changing retries are unique.
The shared ingestion queue is bounded and prioritizes critical audit traffic;
it does not promise per-owner telemetry fairness or complete trace capture.
One flush partitions accepted facts by authenticated `(owner, session)` and
commits each partition in its own transaction. Per worker, active partition
transactions are capped by the configured ceiling, one half of the pool,
and the pool maximum minus a two-connection reserve. Each term has a minimum
of one so one-connection test or emergency pools can still make progress; that
small-pool exception cannot reserve foreground capacity. Event rows, causal
edges, the exact session counter delta, inserted session-end effects, and
config-version projections share that partition transaction. A blocked or
retryable partition therefore cannot retain locks for, roll back, or replay an
unrelated session. Successful and permanently rejected partitions leave the
retry set immediately; only unresolved partitions retain their already-
accounted queue payloads. This per-worker cap limits connection amplification
but is not a shared-pool reservation or a fairness guarantee across workers or
server processes.

The worker coordinator keeps database attempts and retry timers outside its
receive path. Accepted facts enter a per-owner/per-session FIFO; only one
attempt for that key may be active, and a failed head remains ahead of later
facts for the same session. Runnable owners rotate first, then their runnable
sessions, so one owner with many hot sessions cannot consume every dispatch
turn. `batch_size` bounds one session transaction. A sparse session becomes
runnable at an absolute deadline established by its oldest buffered fact;
later arrivals do not reset that deadline. Retry backoff consumes neither a
database slot nor the receive loop. This is process-local dispatch fairness,
not equal SQL execution time or cluster-wide fairness.

Every accepted fact owns one admission lease from before channel entry through
queued, retrying, and in-flight states. Global, per-owner, and per-session
limits apply to both event count and compact-JSON bytes; one blocked session or
owner therefore cannot consume all process-local headroom. A maximum event size
rejects oversized payloads before acceptance. Lease release is tied to terminal
drop rather than scheduler bookkeeping, so cancellation and closed-channel
paths return capacity as well.

Database attempts use a process-local limiter that can be shared by all workers
over one pool. A whole-attempt client deadline includes connection acquisition,
transaction work, and commit. If an exchange times out, the physical connection
is detached and closed rather than returned to the idle pool; acknowledged
commits remain terminal even if later cleanup fails. The limiter and deadline
do not establish cross-process fairness or database-cluster capacity.
The client deadline also does not establish a server-side rollback deadline:
discarding a timed-out connection can restore logical pool capacity before the
server releases transaction locks. Consequently, unrelated session writes must
not share a transaction on the assumption that socket cancellation bounds their
lock coupling. Independent session transactions preserve that isolation boundary.

Enqueue-to-terminal latency uses a fixed-size process-local histogram rather
than retaining per-event samples. A terminal outcome is commit, durable
admission rejection, or explicit shutdown abandonment; retryable attempts keep
their original enqueue timestamp. Shutdown seals the receiver before draining,
so deferred sends that never entered the channel remain pre-acceptance drops.
If the runtime deadline expires, it aborts and awaits the worker instead of
detaching a task that may still own pool resources. Delivery accounting belongs
to the shared admission lease: channel acceptance precedes dispatch, and a
known commit or rejection settles the lease before control returns to the
scheduler. Explicit shutdown failure or final-owner drop counts an accepted,
unsettled delivery as unresolved exactly once across retry clones. Residency
snapshots are not terminal evidence and never add to that count.

Request-classification observations use the existing `trace_span` envelope
with name `semantic_judgment` and a bounded typed JSON string in
`attrs["semantic_judgment.v1"]`. They do not create another usage or execution
ledger. Initial classification and clarification are separate semantic stages,
not a count of physical provider attempts. A classification that succeeds before
planning fails remains a successful classification, not a failed judgment.

The payload contains closed reasons, normalized answer values and provenance,
run/turn/round correlation and a preflight evaluation identity. It excludes raw
provider responses, prompts, tool output, parser error text and credentials.
Trace shape alone is not producer authentication; these facts cannot authorize
execution or settlement. Readers use authenticated owner/session storage scope,
bound bytes before decoding, and exclude conflicting observation or terminal
evaluation identities. Optional database reads use the existing
cancellation-safe connection boundary.

Known preparation failures are not-dispatched facts. Transport failure,
cancellation and timeout do not by themselves establish whether a request was
dispatched; delivery remains unresolved unless response receipt is known.
Classification results do not authorize execution or prove model adoption or
quality improvement. Success does not invent scores absent from the classifier's
result. Unsupported old semantic payloads are rejected rather than migrated.

Trace delivery remains lossy. Successful empty reads do not prove inactivity;
query truncation, display omission and potential upstream loss remain distinct.
Physical attempts and token totals continue to come from the inference ledger.

## Agent event field requirements

Agent event storage should support the following logical fields, whether physically normalized or stored with indexed metadata:

```text
event_id
user_id
session_id
run_id
parent_run_id
turn_id
turn_seq
round_index
tool_call_id
event_type
trace_kind
provider_id
capability
cause_event_id
parent_event_id
created_at
server_received_at
payload_hash
redaction_status
retention_class
metadata
```

`event_id` must be stable and collision-resistant. If the same event id arrives with different payload hash, ingestion must treat it as a collision, not idempotent success.

Durable capture distinguishes `Inserted`, `Replayed`, and `Collision` within the
owning database transaction. Only `Inserted` may apply event counters, parent
edges, terminal-session effects, configuration projections, or manifest artifact
references. An exact retry after a lost commit acknowledgement therefore does
not repeat those effects. Each database attempt uses a fresh write marker;
retries must not reuse a marker from an attempt whose commit outcome is unknown.

Capture outcomes remain available to downstream projections. A collision must
not create a transcript row or snapshot link from the rejected payload, nor be
reported as a successfully captured response. Exact replay may repair a missing
projection from the accepted payload under the existing transaction and session
fences. Valid sibling events in the same batch continue to be captured.

Atomic run-terminal settlement is stricter than ordinary batch capture: every
canonical event in the settlement (including user intents, rounds, and tool
events) must be inserted or exactly replayed. Any collision rolls back the
settlement before terminal status, usage, or transcript changes can commit.
Initial commit and lost-acknowledgement recovery therefore require the same
complete evidence; recovery must not relax hash verification to accept a subset.

Manifest identity and item identity are tenant-scoped: `(user_id, manifest_id)`
and `(user_id, manifest_id, item_order)`. The manifest digest covers its header
and the complete item set ordered by `item_order`. Every item lookup, join, and
delete must carry the owner. Unknown-reason diagnostics commit with the first
manifest capture and are not repeated by replay or collision.

Collision receipts retain identities and hashes, never observation payloads.
They aggregate into one row per `(user_id, identity_kind, identity_id)`, with a
count and latest conflicting hash. Their fixed seven-day expiry is not extended
by repeated conflicts. The bounded runtime-maintenance sweep removes expired
receipts, and explicit session deletion removes its owner-scoped receipts.

The v83 core schema requires capture hashes and attempt markers on event and
manifest writes. Deployments using an earlier table shape require a fresh-schema
cutover; startup rejects missing capture columns rather than assigning empty
hashes to old rows. Hashes include producer occurrence timestamps. Root
execution freezes the turn-start timestamp before its first durable write, then
freezes the terminal offset after execution. Delayed retries and
lost-acknowledgement resolution reuse both bounds, not a later event-buffer
start or the current clock. Content and lineage still undergo full hash
comparison. Configuration
version pushes are the exception: their envelope has a generated delivery time,
so that field is hashed as JSON null and the first stored delivery time is kept.

## Event ingestion unhappy paths

| Path | Required behavior |
| --- | --- |
| Invalid payload shape | Reject or quarantine without poisoning unrelated records. |
| Missing causal fields | Accept only if event type permits; otherwise degraded/quarantine. |
| Oversized metadata | Store summary/artifact ref or reject according to policy. |
| Redaction failure | Fail closed. |
| Duplicate same hash | Idempotent. |
| Duplicate different hash | Collision/poison. |
| Unknown event type | Store only if policy allows extension; otherwise quarantine. |
| Retention class missing | Apply safe default, not infinite retention. |

## Observation metrics

```text
agent_events_ingested_total
agent_events_rejected_total
agent_events_quarantined_total
agent_event_collision_total
trace_events_by_type_total
introspection_requests_total
reflection_requests_total
debug_bundle_created_total
debug_bundle_access_total
debug_bundle_expired_total
```
