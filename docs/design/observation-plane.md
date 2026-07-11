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
