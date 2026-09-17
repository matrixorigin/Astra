# Multi-session and multi-server scale

> Status: staged implementation contract.
> Last updated: 2026-09-15.

This document owns the capacity and availability contract for deployments with
many users, many sessions, and more than one Astra Server. It describes the
limits that must agree before a capacity claim is meaningful. Durable run,
session, Edge, and observation semantics remain owned by their existing design
documents.

## Product target

An Astra Server deployment should be able to keep hundreds or thousands of
sessions connected without turning an idle session into a runtime task. A
session that is actively executing is a separate capacity unit from a session
that is waiting for a user, Edge, or provider. Increasing the number of server
pods must increase execution capacity only when the provider and database
budgets also allow it.

The target is measured with real workloads. “A thousand sessions” is not one
number: idle connections, waiting runs, short turns, long contexts, slow tools,
and active provider calls exercise different limits.

## Capacity layers

Each deployment declares one capacity snapshot:

| Layer | Scope | Meaning |
| --- | --- | --- |
| Run slots | one server process | Maximum agentic loop tasks executing on that process. |
| Cluster run slots | all server processes sharing the database | Maximum outstanding canonical turn reservations. |
| Owner share | one user across the deployment | Fair-share ceiling for canonical turn reservations. |
| Database pool | one process and the database cluster | Connections available to admission, leases, durable events, Edge relay, and reads. |
| Provider budget | external provider | RPM/TPM and actual provider concurrency. |

`ASTRA_RUN_CONCURRENCY_LIMIT` controls per-process run slots. The cluster
budget is derived from that limit and `ASTRA_CAPACITY_POD_COUNT`, unless an
operator supplies a deployment-specific capacity policy. The durable weighted
admission gate stores a hash of the complete weighted budget. A pod with a
different budget fails closed while reservations are active; this prevents a
rolling deployment from silently changing the meaning of an existing global
limit.

The hash check is part of the current admission protocol. Every server sharing
the durable scope must use the same protocol and declared snapshot. A new
scope initializes its hash on the first reservation; an uninitialized hash is
never adopted while reservations are active. A changed budget rotates only
after the active reservations have drained, so a rollout must keep one
capacity snapshot across all participating servers.

Only the provider-slot dimension scales with the declared pod count. Resident
memory, context, CPU, and I/O budgets describe the shared deployment budget and
must not be multiplied merely because more pods were added. Provider capacity
evidence and database capacity are rollout inputs, not optimistic defaults.

## Horizontal scaling invariant

HTTP, SSE, WebSocket reconnect, and status reads may land on any server. The
database remains authoritative for session heads, run ownership, turn
reservations, events, checkpoints, interactions, and Edge dispatch results.
Process-local maps and channels are delivery accelerators only. They may be
lost on restart and must never be required to prove ownership, idempotency, or
completion.

The existing owner lease and durable Edge dispatch relay own cross-pod
execution. A new scheduler, sticky-session requirement, or process-local
parallel state machine must not be introduced to make a scale test pass.

## Admission and failure behavior

- A run waits only at the existing bounded admission boundary; the local
  durable-gate queue, the database-pool acquire, and the cross-server gate
  lock share one run admission deadline. The HTTP request must not hold a
  database connection while waiting in the local queue.
- A distributed capacity rejection is explicit and typed, with retryable HTTP
  semantics and metrics. It must not look like a provider failure.
- If the admission deadline expires before the durable reservation can be
  decided, the request returns an explicit retryable admission-timeout error;
  cancelled local waiters leave no semaphore permit behind.
- Cancellation releases local and durable admission promptly. TTL cleanup is a
  recovery path, not normal capacity accounting.
- A database or provider outage preserves durable run state and exposes a
  degraded reason. It must not create an unbounded in-memory queue.

## Measurement gates

Every capacity change reports, per workload and per pod count:

- admission attempts, rejection reason, wait p50/p95/p99;
- database pool acquire wait, timeouts, in-use and idle connections;
- weighted reservation count and renew/release latency;
- run RSS and retained live-event bytes;
- provider request rate, token rate, time to first token, and error rate;
- durable event/control-plane QPS and end-to-end turn latency.

The first implementation stage aligns local and durable admission configuration
and records the configuration in the shared gate. The second stage keeps exact
global and per-owner usage in the durable protocol: normal reserve and release
mutate those counters in the same gate transaction, while expiry or an explicit
session cleanup marks them dirty and the next admission rebuilds them from the
reservation rows. Rust parses the decimal totals as `u64` and rejects negative
or out-of-range stored rows, so the optimization does not change the capacity
invariant. The rebuild is a repair path; the steady-state decision is O(1) in
the number of active reservations.

The local controller has one async admission permit because one durable scope
has one gate row. Requests wait before acquiring a database connection, so a
burst cannot consume the whole pool while queued behind that row. Renewals and
releases use the same boundary. The durable gate remains the cross-server
serialization point; it is therefore a measured throughput boundary for a
deployment that raises the cluster budget high enough to admit every request.
Scaling that case further requires a sharded or lease-based capacity protocol,
not a larger SQL pool or an early rejection cache.

The third stage adds a four-pool, 1000-attempt MatrixOne harness. It proves that
independent server pools share the same durable global and owner budgets, keeps
successful permits live until all attempts finish, and checks that release does
not leak a reservation. The admission latency it prints includes pool and gate
wait; it is not a full server turn or provider throughput metric. The current
hot path also reads the gate clock and capacity hash with one locked query,
removing a redundant round trip without weakening configuration fencing.

The next stage may introduce sharded/lease-based capacity only after measured
lock wait, pool wait, or reservation-scan evidence justifies it, with crash and
expiry tests.

## Required scenarios

The capacity harness must cover at least 1, 2, and 4 server processes and 1000
sessions owned by 100 users across idle, short-turn, long-context, and
slow-tool workloads. It must assert that the durable reservation total never
exceeds the configured cluster budget, owner share is respected, cancellation
returns capacity, and killing an owner pod does not duplicate a tool or turn.
