# Edge-cloud execution

> Status: target design contract.
> Last updated: 2026-09-03.

Edge-cloud execution defines how cloud backbone state and user-local capacity compose into one agent experience.

## Ownership

This document owns:

- cloud vs edge responsibility split;
- provider composition between cloud and edge;
- offline/degraded execution behavior;
- high-level sync dependency for edge facts.

It does not own:

- detailed provider decision shape, owned by [capability-system.md](capability-system.md);
- runtime safety boundary, owned by [edge-runtime-tool-boundary.md](edge-runtime-tool-boundary.md);
- durable outbox details, owned by [../architecture/edge-cloud-sync-architecture.md](../architecture/edge-cloud-sync-architecture.md);
- Web UI behavior, owned by [web-agent-runner.md](web-agent-runner.md).

## Principle

```text
Cloud owns durable backbone state.
Edge owns user-local capacity.
Provider decisions connect them.
```

Edge is not a second agent brain. Cloud is not a fake local machine.

## Cloud responsibilities

- session/run/turn/task control state;
- transcript and durable event facts;
- context assembly and memory retrieval;
- model routing and prompt-cache stable prompt construction;
- provider decision and audit/trace persistence;
- server-safe tools such as configured `web_fetch`, cloud artifacts, control-plane actions, and request-scoped MCP;
- retention, redaction, and learning lineage policy.

## Edge responsibilities

- local workspace authority;
- shell, file, git, local MCP, local browser/network context;
- local permission enforcement;
- local journal;
- durable sync outbox source;
- provider health and capability advertisement.

## Composition model

```text
client surface
  -> cloud backbone
  -> provider decision
  -> server-safe provider or edge provider
  -> result envelope
  -> trace/audit/transcript
```

The model never switches to a separate edge-only semantics. Edge results re-enter the same trace, audit, transcript, and checkpoint model.

## Provider priority

When the same capability exists on Edge/CLI and Server, prefer Edge/CLI by default. This is especially important for `web_fetch`, because local identity, network, cookies, VPN, and user environment may matter.

Server fallback is allowed only when policy permits it and must be recorded in trace.

## Offline and degraded behavior

| Condition | Behavior |
| --- | --- |
| Edge offline | Keep backbone alive, mark provider offline, expose reconnect/fallback options. |
| Edge reconnecting | Avoid dispatching unsafe local tools until binding is ready. |
| Server fallback allowed | Execute fallback, persist provider fallback decision. |
| Server fallback denied | Block affected capability, not the whole session. |
| Sync degraded | Continue only for safe operations and expose sync state. |

## Edge connection identity and lifecycle

An Edge connection is a capability-provider binding, not merely an authenticated
socket. Registration credentials and ordinary runtime-request credentials are
distinct authorities. Admission must bind the authenticated principal to the
declared user, workspace, and Edge identity; a self-reported identity cannot
override the authenticated binding.

Connection publication follows these invariants:

- authentication succeeds and its acknowledgement is delivered before the
  connection becomes dispatchable;
- durable registration failure leaves no active in-memory route;
- reconnect uses a generation-fenced handoff so stale cleanup cannot remove a
  newer connection;
- heartbeats and disconnect cleanup are scoped to the exact connection
  identity and generation;
- an Edge is not eligible for unsafe local dispatch until registration and
  capability binding are both complete.

These rules apply to native User Runners and externally provisioned workspace
runtimes. Provider-specific token formats belong to authentication
configuration, not this execution contract.

## Durable sync dependency

Edge-originated facts that must survive reconnect require durable outbox semantics. A local journal or in-memory ingestion channel is not enough for cloud-edge correctness.

## Test obligations

- reject a registration whose authenticated and declared Edge identities do
  not match;
- prove that failed durable registration cannot leave a dispatchable route;
- exercise overlapping reconnect and stale-disconnect ordering;
- verify workspace-scoped dispatch and offline/degraded behavior.

## Edge disconnect settlement and result transport

Every connected Edge owns and joins its execution tasks. Socket and journal
errors pass through the same cancellation/settlement path as a clean close.
The executor must be allowed to finish asynchronous process-group cleanup;
dropping its future when a token fires is not cancellation settlement.
Reconnect cannot begin while the previous connection's tasks remain active.

`timeout_secs` is the deadline for requesting cancellation, not a deadline for
abandoning cleanup. Edge uses the canonical executor's cancellation contract:
generic asynchronous dispatch is cancellation-aware in the executor wrapper;
Bash and run_script must terminate/reap children and settle workspace observation
before returning. New executor paths must preserve this contract. A synchronous
non-yielding operation cannot be made preemptible by wrapping it in a Tokio
timeout. Do not add a cleanup timeout that merely drops the future and allows a
new connection to overlap an unproven old execution.

After cancellation the connection drops its completion sender and drains the
remaining senders while tasks finish, then joins their handles. It persists
actual success/error/cancellation evidence before reconnecting; queued results
are not discarded or rewritten as unknown merely because the socket closed.
Draining must continue even after a persistence failure to unblock senders.
Following a journal I/O or integrity error, no further append is attempted on
that connection; reopening validates the WAL and recovers records without a
durable completion as unknown. A connection reconnect does not imply a process
restart. The old body-only limit is retained solely as a legacy read integrity
check; the full wire budget owns all new terminal writes.

The shared `MAX_EDGE_MESSAGE_BYTES` budget measures the complete serialized
message, including identity, generation, metadata and escaping. The journal
persists an explicit unknown-evidence result if a completed result cannot fit.
On open it applies the same durable transition to legacy pending results that
fit the former body-only limit. Results remain pending until an exact ACK;
this transition never requests re-execution of the original tool.

Release candidates explicitly initialize the Rustls provider before TLS clients
are constructed. The joint CLI/Edge build runs an offline direct/CONNECT
certificate-rejection probe on its actual Edge binary. This probe is necessary
but does not replace successful WSS registration or live workflow acceptance.
