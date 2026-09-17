# Session observability

> Status: target design contract.
> Last updated: 2026-07-07.

Session observability defines how users, agents, and support tooling understand what is happening in a session without reading raw logs.

## Ownership

This document owns:

- user-visible session/run/task status;
- stream event projection semantics;
- stuck/degraded/blocked explanations;
- observability views for resume, cancel, and reconnect;
- support-grade status summaries.

It does not own raw trace schema, which belongs to [observation-plane.md](observation-plane.md).

## Principle

```text
A session should be explainable from durable projections and structured facts.
```

## Status model

A session status projection should include:

```text
session_id
active_run_id
run_status
stage
current_turn
active_task_summary
provider_status
sync_status
last_progress_at
waiting_for
blocked_reason
resumability
terminal_outcome
```

## Progress semantics

Progress should be derived from durable events:

- model round started/completed;
- tool call started/completed/failed;
- provider decision;
- retry decision;
- cache decision;
- sync status;
- task transition;
- checkpoint saved;
- stream cursor advanced.

## Stuck detection

A run may appear stuck because of:

- model TTFB;
- provider offline;
- tool timeout;
- permission wait;
- plan-mode block;
- sync degraded;
- owner lease issue;
- stream disconnect;
- task waiting for child run.

The projection should expose the specific reason and next action.

## Stream events

Stream events are transport projections of durable or near-durable facts. They should carry enough information to repair UI state after reconnect.

Tool terminal projections preserve execution facts independently of display
previews. `executed` describes this call: `false` means no execution, `true`
means execution occurred, and explicit `null` means execution cannot be
confirmed. An absent field supplies no execution fact. A `reused` disposition
can carry a receipt from an earlier execution without executing this call
again. Live delivery, replay, and size-bounded projections preserve these
distinctions. A shortened preview is not an authoritative receipt; control
identities remain exact or are explicitly omitted, never shortened into a
different identity.

Malformed non-critical stream events should be isolated when possible. Identity or cursor corruption should fail closed with structured error and should not corrupt durable run state.

## Tool results and runtime guidance

Tool result documents and runtime-authored guidance are distinct evidence.
Recovery, quality, duplicate-observation, and pre-tool context annotations must
not be appended to `ToolCallRecord.result_full`: doing so can turn complete JSON
into an invalid document and break typed consumers or artifact recovery.

The journal retains `runtime_advisories` beside the result. Canonical tool
messages retain the same per-call guidance in `_astra_tool_result_advisories`,
including across continuation and compression. Executor-provided metadata cannot
author this reserved runtime field. Guidance is rendered only on disposable
provider/display projections; projection consumes the internal marker exactly
once and canonical-suffix checks use the same projection. Model-facing content
may contain explanatory prose; the canonical result document remains unchanged.
This adds no volatile singleton, global cache prefix, or separate replay state.

Oversized guidance uses the same immutable artifact store and bounded
`introspect(artifact, offset, max_bytes)` recovery as oversized output. Artifact
identity includes the document kind: ordinary result and runtime guidance for
the same run/call cannot overwrite one another. Ordinary result handles and
paths are unchanged. Only result-kind descriptors can authorize `result_full`
or result compaction. The journal retains `runtime_advisory_artifact` beside
the bounded guidance preview; full guidance remains in that verified artifact,
not in every future prompt or journal row.

## Resume and reconnect

On reconnect, the client should rebuild from:

- latest run status;
- transcript cursor;
- task projection;
- provider status;
- sync status;
- recent trace summary;
- artifact manifest.

Browser disconnect is not cancellation.

## User-facing diagnostics

A user should be able to answer:

- Is it running, blocked, waiting, cancelled, or complete?
- What is it waiting for?
- Is Edge/CLI/server/MCP provider available?
- Did sync finish?
- Can I resume, retry, reconnect, or cancel?
- What changed since last visible output?

## Test obligations

- Offline provider produces visible blocked/degraded status.
- Malformed stream cursor does not corrupt active run state.
- Browser disconnect does not imply cancellation.
- Cancelled tasks disappear from active task board projection.
- Resume reconstructs status without stale UI cache.
