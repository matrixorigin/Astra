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

Fresh run-control observations share an on-demand read queue in `RunEngine`.
Concurrent requests across owners use at most 128 positional lookups per SQL,
four concurrent queries, and a maximum 25 ms collection window measured from
admission. Full batches and partial batches whose oldest request has already
waited that window dispatch without another collection delay.
The queue admits at most 8,192 outstanding reads and 4,096 per owner, including
in-flight queries, with bounded identity bytes and owner-rotating dispatch.
Database equality associates each request with its result; missing or malformed
rows do not leak into another request. A bound candidate run-ID filter keeps
unrelated historical runs out of the base-row join work; query-count reduction
must be validated together with the actual scan plan. Observations are never
cached, polling intervals and ancestor controls remain unchanged, and no idle
queue issues SQL.
Overload and storage failures are explicit; this queue is not execution
authorization. Lease renewal and provider/action fences retain their owners.
The collection window is not a total-latency guarantee: query attempts have a
three-second deadline and callers have a four-second overall read budget.

Direct-child completion waits are a delivery concern, not a new lifecycle
authority. Local children notify their parent in memory. Remotely owned
children are observed by one Server-instance coordinator that batches exact
owner/session/run identities across waiting sessions; it reads existing
durable facts and only wakes the affected parents. A parent must not start
its own fixed-interval database poll. With no remote waiters, the coordinator
makes no database requests. A wakeup is only a hint: the parent revalidates
the durable status and result before final synthesis. Capacity evidence must
count the pre-existing run-control and live-stream observers as well as this
coordinator, rather than presenting the coordinator's isolated query rate as
the deployment total.

A parent awaiting children must not retain an execution permit, weighted
admission reservation, full agentic-loop state, or per-run control poll for the
duration of the wait. It may retain the logical session-writing exclusion, but
must durably record a continuation before releasing execution ownership. A
child-completion hint resumes that continuation through normal capacity
admission and generation fencing; it cannot synthesize an answer from a stale
in-memory parent. Cancellation and explicit user pause take precedence over
automatic resume. A normal conversation with no child wait does not pay this
checkpoint or observer cost. Merely batching child polls while leaving the
parent loop alive does not meet the multi-session scale target.

The staged shared observer does not yet satisfy this whole invariant: parent
parking, durable same-run continuation, and automatic re-admission remain
required before claiming scale for 1,000 waiting parents. The execution handoff
preserves the original absolute work and total deadlines; restoring those
cutoffs must use the current wall clock, not grant another remaining-duration
budget or recompute the synthesis reserve. This deadline snapshot is not an
executable continuation or evidence of released admission capacity.
Current recovery
also fails explicitly when one selected set exceeds 1,024 exact IDs or its
event projection exceeds 2,048 rows, 16 MiB total, or 2 MiB per event. These
limits prevent silent omission and unbounded loading; they are not a substitute
for paged discovery and a completeness-aware parent barrier for very large
fan-in.

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
- for 1,000 waiting parents: execution permits and weighted reservations held,
  retained parent state bytes, observer queries per second, resume queue depth,
  and ordinary-turn admission latency while children complete in a burst.

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

## Durable admission partitions

The existing database controller uses 16 fixed owner partitions, encoded in the
existing scope key as `canonical_turn_v2_00` through `canonical_turn_v2_15`.
SHA-256 of the exact owner ID bytes selects the partition; all domains and
sessions of that owner use it on every pod. Partition identity is not a pod
assignment or a new routing service. The three admission tables and their
existing compound keys remain the only authority.

Each gate stores a five-resource allocation, exact charged usage, configuration
hash and conservative expiry frontier. Allocations sum componentwise to the
configured global budget, charged usage cannot exceed its partition allocation,
and each domain/owner retains its original share limit. Initial allocations
split each resource by quotient and remainder. Credits are durable: a pod crash
does not destroy or reissue them.

Ordinary reserve, renewal and release acquire only their owner partition gate.
A local semaphore per partition queues requests before pool checkout. A missing
configuration or allocation miss releases that transaction and local permit.
One local refill semaphore then coalesces credit/configuration work before any
checkout. A credit miss reads its fresh snapshot only after this queue; a
configuration miss instead rechecks the owner without a probe. A valid snapshot
may suggest the target and donors sufficient for the deficit. The controller
locks only these database gates in ascending order and revalidates real credit.
A stale suggestion ends its transaction before one full-inventory attempt,
retaining only the refill semaphore. Refills do not acquire owner-local permits
or spend capacity from local state. Every attempt uses the
same transaction implementation, repairs due/dirty partitions and checks exact
replay and owner/global limits.
It transfers only unused donor credit and grants atomically. Per deficient
resource, the transfer targets the greater of the exact deficit and one
sixteenth of global capacity, capped by available donor credit. Fragmentation
must not cause a false global-exhaustion decision. No background refill,
per-pod lease, local capacity cache or singleton fallback exists.

Before local escalation, a credit miss may be rejected by one fresh read-only
transaction. Its single SQL snapshot must simultaneously prove all 16 gates
configured, clean, conserved and not due for expiry, and the exact logical
idempotency identity absent. It may reject or suggest donor locks, never grant
or transfer credit; the ordered locking protocol remains authoritative. No old locking
snapshot, separate identity read, cached result or additional pool checkout
while retaining the original transaction can substitute for this proof.

Configuration initializes or rotates only while no reservation remains across
the locked partitions. All configuration fingerprints include the partition
contract. Repair and session deletion affect only the owner's partition;
multi-gate transactions always acquire gates before reservation/owner rows.
Renew/replay/TTL decisions sample database time after the final lock, inside the
original deadline and cancellation-safe transaction. Release needs no clock.
Exact logical identities and generation UUIDs fence stale renewals and releases.

A future expiry frontier avoids a reservation scan. Due/dirty repair rebuilds
only the locked partition and parses wide decimal sums without signed overflow.
Successful cleanup commits even when admission is rejected. Replays remain
valid at full capacity without a provisional INSERT; changed work is a mismatch.

This removes the common row from the steady-state path, not all contention.
Same-owner/hash-collision traffic still serializes. Initialization and stale
donor fallback briefly coordinate all partitions. Physical gate count is fixed at 16; active
reservation count remains bounded by the configured provider-slot budget.
Performance claims require matched multi-process measurements; pool count is
not pod count, and admission throughput is not complete-agent throughput.

## Required scenarios

The capacity harness must cover at least 1, 2, and 4 server processes and 1000
sessions owned by 100 users across idle, short-turn, long-context, and
slow-tool workloads. It must assert that the durable reservation total never
exceeds the configured cluster budget, owner share is respected, cancellation
returns capacity, and killing an owner pod does not duplicate a tool or turn.
