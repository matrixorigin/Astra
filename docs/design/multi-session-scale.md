# Multi-session and multi-server scale

> Status: staged implementation contract.
> Last updated: 2026-09-27.

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
Stored sessions and previously opened chats do not create run-stream polling;
observer read load follows concurrently attached live runs and their subscribers.

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

Run observers read an owner-scoped status and event high watermark before
fetching any event tail. A caught-up cursor needs no event-table scan. A tail
read stops at the captured high watermark; a concurrent append is delivered
on a later poll. SSE live attach polls promptly while events arrive and backs
off to at most one second between reads when idle. The database remains the
source of truth for reconnect and cross-pod delivery. This bounded display
delay must not weaken durable event ordering, terminal status, or Explain
publication outcomes.

Browser WebSocket clients authenticate in the first frame, then may send
`attach_run` with an owned `run_id` and inclusive `last_index` cursor after a
disconnect. The server verifies run ownership before binding the connection
for event replay and durable approval or ask_user responses. Clients replay
the last seen event index because one durable event may produce several wire
frames, and suppress frames they already displayed. The `session_info` frame
accepts an attachment. A retryable lookup failure closes the socket so bounded
reconnect can retry; a terminal rejection leaves the authenticated connection
available for a different run. WebSocket terminal delivery follows the same
bounded Explain publication reconciliation as SSE;
the artifact publication or explicit unavailable outcome precedes the final
`run_finished` frame. A replayed terminal derives failure from its own durable
error facts even when the preceding `run_error` is before the reconnect cursor.
Clients accept that bound terminal at its original lower event index. Until it
arrives, reconnect replay starts before the publication
so the original terminal and its owner generation remain recoverable. The
bounded run-status snapshot includes the root Explain request flag from the
already-read `run_started` fact. Ordinary non-Explain WebSocket terminals,
including reattachments, deliver directly from the captured tail without
hydrating the full run history for publication reconciliation.

SSE observer demand follows the run lifecycle:

| Run and client state | Durable observation behavior |
| --- | --- |
| Historical session without a live attachment | No recurring run read. |
| Running or waiting run with an attached client | Follow its cursor; an empty tail backs off. |
| SSE client disconnects | Stop that attachment; the run remains durable and can be reattached by cursor. |
| Paused or ordinary terminal run | Deliver the captured tail and close the attachment. |
| Terminal Explain run awaiting publication | Keep the bounded publication grace, perform a final durable check, then deliver the real publication or an explicit unavailable outcome. |

Pod loss and reconnect use the same owner-scoped durable cursor. A local
notification can shorten delivery latency but cannot replace that read.

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

Measure observer read QPS separately for idle, active, and terminal runs, with
SSE and WebSocket attachments on both owner and other pods. Include event-tail
bytes, poll-to-display delay, reconnect replay completeness, and Explain
publication delay. The read optimization has no capacity claim until those
measurements are taken under the required multi-user scenarios.

### Observation-ingestion capacity

Connected or active sessions must not map one-for-one to database connections
or background tasks. Durable observation ingestion uses logical owner/session
queues and a bounded number of database attempts. One session has at most one
attempt in flight so retries cannot be overtaken; runnable owners rotate before
their runnable sessions. This process-local ordering improves isolation but is
not evidence of cluster-wide fairness.

Admission covers every accepted payload state: deferred send, channel,
scheduled tail, retry head, and in-flight transaction. The admission lease is
bounded by both serialized bytes and event count and includes per-owner and
per-session shares, so one blocked producer cannot consume all global
headroom. Database-attempt permits are shared by every ingestion worker using
the same process pool and remain held until transaction cleanup has either
completed or discarded the physical connection. A retry timer does not own a
database permit.

A finite burst that eventually persists 1,000 single-event sessions is only a
smoke/load baseline. A sustained-capacity claim additionally requires declared
open-loop offered rate and duration, repeated and mixed-size payloads, hot-owner
competition, injected transaction failures, all attempt slots blocked, and a
foreground database workload. Report accepted, committed, rejected, and
unresolved counts; retained event bytes; effective database concurrency;
oldest-backlog age; owner progress; pool-acquire latency; and end-to-end
ingestion p50/p95/p99. Multi-process or cluster claims require the same evidence
at that deployment scope.

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
