# Multi-session and multi-server scale

> Status: staged implementation contract.
> Last updated: 2026-10-24.

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
| Cluster run slots | deployment planning | Sum of process limits; no exact database quota. |
| Owner share | one user on one process | Hard ceiling within the process admission budget. |
| Database pool | one process and the database cluster | Connections available to admission, leases, durable events, Edge relay, and reads. |
| Provider budget | external provider | RPM/TPM and actual provider concurrency. |

`ASTRA_RUN_CONCURRENCY_LIMIT` controls per-process run slots. Cluster estimates
multiply by `ASTRA_CAPACITY_POD_COUNT` for planning only. Admission does not
store a deployment budget, reserve database credits or coordinate budget hashes
across pods. Each process enforces its own resource and owner ceilings; routing,
provider capacity and database capacity determine effective deployment capacity.

## Horizontal scaling invariant

HTTP, SSE, WebSocket reconnect, and status reads may land on any server. The
database remains authoritative for session heads, Run ownership and leases, explicit Controller authority, events, checkpoints, interactions, and Edge dispatch results.
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

- A run waits only at the existing bounded process-local admission boundary,
  with an explicit deadline. It holds no database connection while waiting.
- Capacity rejection is explicit, typed and retryable; it must not look like a
  provider failure. Cancelled waiters leave no local permit behind.
- Cancellation and execution drain release process-local RAII capacity. Durable
  Run ownership and effect custody remain independently fenced; no database
  capacity reservation, cross-server gate or capacity TTL cleanup is required.
- A database or provider outage preserves durable run state and exposes a
  degraded reason. It must not create an unbounded in-memory queue.

## Measurement gates

Every capacity change reports, per workload and per pod count:

- admission attempts, rejection reason, wait p50/p95/p99;
- database pool acquire wait, timeouts, in-use and idle connections;
- process-local weighted permits, retained resources and release latency;
- run RSS and retained live-event bytes;
- provider request rate, token rate, time to first token, and error rate;
- durable event/control-plane QPS and end-to-end turn latency.
- for 1,000 waiting parents: process-local execution permits and weighted resources held,
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

## Process capacity

Each server owns a process-local weighted admission controller. A turn holds
one RAII permit for resident bytes, context tokens, provider slots, CPU work
and canonical I/O. Owner ceilings leave room for other tenants. Admission
rejects excess work before provider dispatch; cancellation and execution drain
release the permit. There are no database reservations, gate rows, expiry
repairs, credit transfers or per-turn accounting transactions.

`ASTRA_RUN_CONCURRENCY_LIMIT` controls this process's provider slots.
`ASTRA_CAPACITY_POD_COUNT` is a planning input only: adding pods does not enlarge
a single process's capacity. Aggregate cluster capacity depends on placement
and routing; an exact global weighted quota is no longer a product guarantee.
Billing and durable Run/effect ownership remain separate contracts.

## Required scenarios

Exercise idle, short-turn, long-context and slow-tool workloads with competing
owners. Assert local capacity and owner ceilings, cancellation returning
capacity, and one owner unable to monopolize the process. Multi-process tests
must check that each process enforces its own limit and that killing a process
does not duplicate a durable effect or grant a second live Run owner.
