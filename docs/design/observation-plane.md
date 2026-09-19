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
