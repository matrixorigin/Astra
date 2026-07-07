# Cloud-edge sync architecture

> Status: target architecture.
> Last updated: 2026-07-07.

This document defines the durable data sync target for Astra's cloud-edge model. It is not an implementation audit.

## Goal

A user should be able to run an agent across Web, CLI, and Edge without losing backbone semantics or local work continuity.

Cloud is the durable source of truth for shared facts. Edge preserves local continuity and supplies user-local capacity. Sync makes the two converge through explicit facts, not hidden UI state.

## Data layers

| Layer | Stored facts | Default location |
| --- | --- | --- |
| C0 control | session, run, task, checkpoint, lease, execution slot | cloud |
| C1 transcript | user/assistant visible conversation | cloud, edge cache |
| C2 audit facts | run events, agent events, permission decisions | cloud |
| C3 trace facts | model rounds, tool lifecycle, provider/retry/cache/sync decisions | cloud |
| C4 debug bundle | explicit raw diagnostics and manifests | cloud artifact store with short TTL |
| C5 learning artifacts | redacted consent-gated examples and labels | learning store |

Edge may keep local copies, but cloud facts must be reconstructable from durable sync records.

## Durable outbox contract

Every edge-originated fact that must reach cloud goes through an outbox record:

```text
outbox_id
session_id
run_id
local_sequence
event_id
payload_hash
event_kind
payload
created_at
attempt_count
next_retry_at
last_error
state
ack_watermark
```

State transitions:

```text
pending -> sending -> acked
pending -> sending -> retryable_failed -> pending
pending -> sending -> poisoned
acked -> compacted
```

Poisoned records must not block later independent records forever. They must be visible through sync status and repair tooling.

## Cloud ingestion contract

Cloud ingestion must be idempotent:

- stable event id prevents duplicate facts;
- payload hash detects id collision with different content;
- invalid records are quarantined;
- redaction failure fails closed;
- accepted events advance ack watermark;
- ingestion emits trace facts for degraded sync and repair.

## Retention

Retention policy is per layer:

- C0 retained for product/account policy.
- C1 retained as user-visible session history.
- C2 retained for audit policy, then archived or summarized.
- C3 retained for diagnosis and quality windows, then compressed or deleted.
- C4 short TTL by default and explicit opt-in.
- C5 governed by consent, lineage, and deletion propagation.

`agent_events` must not be an infinite append-only table without TTL/archive semantics.

## User-visible sync state

The UI and CLI should expose:

- `synced` when cloud ack is current;
- `syncing` when outbox has pending records;
- `offline` when transport is unavailable;
- `degraded` when retries or dropped/poisoned records exist;
- `action_needed` when user repair or reconnect is required.

The user should not have to infer safety from logs.

## Required operations

- sync status;
- sync retry;
- sync repair;
- export diagnostic bundle;
- delete debug bundle;
- list poisoned records;
- explain last fallback/degraded provider decision.

## Sync operations contract

Sync operations should be available through CLI and Web/API equivalents.

### `sync status`

Returns:

```text
session_id
provider_id
state
last_ack_watermark
outbox_depth
oldest_pending_age_seconds
retryable_failed_count
poisoned_count
last_error
next_retry_at
action_required
```

States:

```text
synced
syncing
offline
degraded
action_needed
blocked
```

### `sync retry`

Retries retryable records without changing poisoned records.

Required behavior:

- idempotent;
- bounded batch size;
- reports accepted, retried, still_failed;
- preserves original event ids;
- emits `sync_status` trace event.

### `sync repair`

Repairs poisoned or conflicting records through explicit policy.

Repair actions:

```text
skip_poisoned
redact_and_retry
export_for_support
rebuild_from_journal
mark_unrecoverable
```

Repair must be auditable and should never silently drop data.

## Sync trace payloads

### `sync_status`

```text
event_type = sync_status
session_id
run_id
provider_id
state
outbox_depth
last_ack_watermark
oldest_pending_age_ms
poisoned_count
retryable_failed_count
action_required
reason
```

### `sync_record_failed`

```text
event_type = sync_record_failed
session_id
run_id
provider_id
outbox_id
event_id
failure_kind
retryable
attempt_count
next_retry_at
error_summary
```

### `sync_record_poisoned`

```text
event_type = sync_record_poisoned
session_id
run_id
provider_id
outbox_id
event_id
poison_kind
repair_options
error_summary
```

## Unhappy paths

| Path | Required behavior |
| --- | --- |
| Edge offline before dispatch | Mark provider offline; do not issue local tool call; expose reconnect/fallback. |
| Edge disconnects during dispatch | Set in-flight dispatch to failed/degraded unless result is durably acked; avoid double success. |
| Outbox disk full | Stop accepting unsafe local facts; report action needed. |
| Outbox channel full | Backpressure or degrade before dropping critical records. |
| Cloud DB unavailable | Keep local outbox pending; retry with backoff; mark syncing/degraded. |
| Duplicate event id same hash | Treat as idempotent success. |
| Duplicate event id different hash | Mark collision/poison; require repair. |
| Redaction failure | Fail closed; do not ingest raw sensitive data. |
| Poison record | Quarantine and allow later independent records when safe. |
| Ack watermark regression | Reject and audit. |
| Clock skew | Preserve local monotonic sequence and use server ingest ordering for cloud facts. |
| Retention expiry | Delete or archive according to layer policy; preserve allowed audit metadata. |
| Owner mismatch | Reject and emit audit event. |
| Corrupt checkpoint | Fall back to earlier checkpoint or transcript/event restore; report degraded. |

## Sync metrics

Required metrics:

```text
sync_outbox_depth
sync_lag_seconds
sync_oldest_pending_age_seconds
sync_retry_attempts_total
sync_poison_records_total
sync_records_acked_total
sync_records_failed_total
sync_records_dropped_total
sync_repair_actions_total
event_ingestion_dropped_total
event_ingestion_poisoned_total
event_ingestion_redaction_failed_total
sync_provider_offline_seconds
```

Metrics must be attributable by provider type, provider id, user/session class when safe, and event kind.

## Test obligations

- Offline Edge preserves cloud backbone and blocks only Edge-bound tools.
- Local outbox survives process crash before cloud ack.
- Retry after cloud DB outage eventually acks without duplicate facts.
- Poison record does not block later independent records forever.
- Event id collision with different hash is detected.
- Redaction failure prevents ingestion of raw sensitive payload.
- Ack watermark cannot move backward.
- Sync status returns action-needed state for poisoned records.
- Browser/Web reconnect can display sync degraded state.
