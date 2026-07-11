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

Malformed non-critical stream events should be isolated when possible. Identity or cursor corruption should fail closed with structured error and should not corrupt durable run state.

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
