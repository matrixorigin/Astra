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

### Web transport boundary

Shared web fetch/search receives its transport policy from the execution owner,
not model arguments. Server uses direct connections with validated, pinned public
DNS addresses and ignores host proxy variables. Explicitly bound CLI and User
Runner execution honors that process's HTTP(S)/ALL proxy and NO_PROXY environment.
The latter trusts the user-selected proxy to resolve/connect safely: HTTP CONNECT
and remote-resolution proxies cannot inherit the direct transport's IP-pinning
guarantee. Both modes validate public destinations and every redirect locally;
proxy-only DNS names are not supported. A failed proxy request never silently
falls back to direct or Server execution. Cache identity includes transport mode.

One total fetch deadline covers DNS, redirects and body reads. Network, timeout,
HTTP and policy failures retain typed failure metadata through search and local
tool callbacks; a failed connection is not reported as an unreadable search page.

## Offline and degraded behavior

CLI SSE consumption executes only typed, admitted Server tool requests.
Assistant text cannot be reinterpreted as a local tool call. Provider-level
degraded call recovery remains on the Server before dispatch.

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
- unavailable registration storage or a lost setup claim closes the WebSocket
  with code 1013 (try again later), allowing the existing Edge reconnect loop
  to retry; it must not become a permanent credential rejection or a host
  process exit;
- reconnect uses a generation-fenced handoff so stale cleanup cannot remove a
  newer connection;
- native admission reads local and durable registration views behind the
  existing per-user/Edge registration barrier, after setup commits or rolls
  back, without extending that lock into inference;
- heartbeats and disconnect cleanup are scoped to the exact connection
  identity and generation;
- an Edge is not eligible for unsafe local dispatch until registration and
  capability binding are both complete.

These rules apply to native User Runners and externally provisioned workspace
runtimes. Provider-specific token formats belong to authentication
configuration, not this execution contract.

## Durable sync dependency

### Sealed live capacity transport

Explicit Edge capacity providers may advertise a bounded public provider ID
and recipient key under `capacity_rpc_v1`. Handshake admission validates and
preserves this declaration; local endpoints and bearer keys never leave Edge.
This is generic transport, not a model tool inventory or another agent loop.
The shared Edge delivery owner handles these connection-scoped frames; a host
adapter supplies only its explicitly configured local provider. Disconnect
cancels and joins live calls without adding them to the invocation journal.

With the durable Edge registry configured, each node publishes its own
operator-configured `ASTRA_EDGE_CAPACITY_RELAY_ORIGIN` (HTTP(S) origin, no path,
credentials, query or fragment). This must identify one node, not a load-balanced
Service. Client-supplied routing metadata is stripped at handshake; only the
server writes `capacity_relay_v1`. A missing node route rejects capacity-bearing
registration. Normal Edge registrations do not require this setting.

`GET/POST /service/edges/capacity` require the existing private service key and
resolve the exact user/workspace/Edge through the canonical registry on every
request. Describe returns the current globally unique registry socket UUID as
an opaque string `generation`. Calls freeze that UUID. A non-owning node forwards
once to the owning node, which rechecks the registry and live socket. Registry
failure is explicit unavailability; replacement is binding changed. No redirect,
retry, durable queue, reconnection replay, alternate executor or payload decoding
is allowed. Loss after dispatch is outcome unknown. The in-process pool counter
continues to own socket cleanup/result matching but is not an external lease.

An explicitly unconfigured single-process registry uses its live pool directly;
this is a declared runtime topology, not recovery after a registry failure.

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
restart. The full wire budget owns both new terminal writes and recovery
validation; there is no separate legacy body-only result limit.

The shared `MAX_EDGE_MESSAGE_BYTES` budget measures the complete serialized
message, including identity, generation, metadata and escaping. The journal
persists an explicit unknown-evidence result if a completed result cannot fit.
On open it applies the same durable transition to any structurally valid
pending result whose complete wire envelope is too large. Results remain
pending until an exact ACK; this transition never requests re-execution of the
original tool.

Release candidates explicitly initialize the Rustls provider before TLS clients
are constructed. The joint CLI/Edge build runs an offline direct/CONNECT
certificate-rejection probe on its actual Edge binary. This probe is necessary
but does not replace successful WSS registration or live workflow acceptance.
