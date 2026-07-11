# Durable agent runs

> Status: target design contract.
> Last updated: 2026-07-07.

Durable agent runs define how agent execution survives long tasks, reconnects, cancellation, owner changes, provider failures, and process crashes.

## Principles

- A run is durable control state, not an HTTP request.
- Ownership is leased and recoverable.
- Checkpoints are correctness artifacts, not only performance optimizations.
- Terminal outcomes must be explicit.
- Sub-runs and delegation preserve lineage.

## Run record

A run should capture:

```text
run_id
session_id
parent_run_id
agent_id
status
stage
owner
lease_expires_at
checkpoint_ref
current_turn
waiting_for
terminal_outcome
created_at
updated_at
```

## Checkpoint contract

Checkpoint must include enough information to resume safely:

- current stage;
- pending tool calls;
- provider decisions relevant to pending work;
- task state refs;
- transcript cursor;
- artifact refs;
- last durable event cursor;
- cancellation/resume policy.

## Lease and ownership

- Only the owner may advance active execution.
- Lease expiry enables recovery.
- Recovery must avoid double execution of non-idempotent actions.
- Session execution slots prevent conflicting root runs when required by product semantics.

## Terminal outcomes

Terminal states should distinguish:

- completed;
- failed;
- cancelled;
- interrupted partial;
- blocked terminal;
- expired;
- superseded.

## Resume

Resume should validate:

- run status;
- checkpoint integrity;
- session/task projection;
- provider availability;
- pending side-effect safety;
- user intent.

Buffered completion may finalize without resuming execution when the answer is already durable.

## Test obligations

- Crash after model output but before final event.
- Crash during tool execution.
- Owner lease expiry and takeover.
- Duplicate resume attempts.
- Sub-run lineage recovery.
- Provider offline during resume.
