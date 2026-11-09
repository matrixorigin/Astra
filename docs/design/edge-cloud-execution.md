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

Fake-IP DNS (a local VPN answering from `198.18.0.0/15`) is blocked like any
reserved address. The user may relax the private-network boundary for the local
transport through the user-level `[web_fetch]` configuration: a fetch whose
answers all lie in the IPv4 fake range goes to the fake address, and the VPN
picks the real destination. A DNS-over-HTTPS screen blocks hosts whose public
answer is private, but it cannot bind the VPN's destination, so the opt-in is an
explicit relaxation rather than a verified boundary. IPv6 fake-IP answers and
answers mixing fake and real addresses are rejected as unsupported. Only the
local transport can carry this opt-in; Server transports are constructed without
it. See the
[configuration reference](../reference/configuration.md#local-web_fetch-behind-fake-ip-dns).

One total fetch deadline covers DNS, redirects and body reads. Network, timeout,
HTTP and policy failures retain typed failure metadata through search and local
tool callbacks; a failed connection is not reported as an unreadable search page.

### Explicit native collaborator selection

The CLI's ordinary Edge ledger and a native collaborator have different
requirements. Ordinary turns do not eagerly probe PATH, create a Session, or
attach a native provider WebSocket. Native collaborator installation is selected
explicitly with `ASTRA_NATIVE_COLLABORATORS=true`, using the same delivery owner
for headless and interactive turns. A selected capacity's readiness is a
precondition for admission; failed installation must not fall through to a chat
request whose Edge registration was withdrawn during cleanup. The existing
invocation-time permission and account/attachment boundaries remain authoritative.

### Shell capability and timeout projection

The selected CLI executor advertises managed background Bash only when its
platform supports it and `ASTRA_ALLOW_ENVIRONMENT_BACKGROUND_TASKS=1` authorizes
it. Execution rechecks authorization before spawning, and reports a structured
non-retryable capability rejection with no-execution evidence when unavailable.
Cleanup guidance uses that same decision and otherwise recommends bounded
foreground work. Server configuration cannot grant a remote Edge capability.

CLI execution and Server admission use the shared compound-aware Bash timeout
resolver. A `cd ... && make ...` command receives the same build allowance as
direct `make`; explicit command timeouts remain bounded by run/policy limits.

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

Durable dispatch subscribes before a connection is published. The existing
per-Server database observer discovers work admitted on another Server or
before subscription; a wake only prompts the socket to claim durable work.
Dispatch stores the serialized request as lossless text. Replay checks the same
owner and serialized payload, preserving numeric types and values. Direct and
relay admission verify the request inside the same transaction; a relay becomes
visible and wakes its Edge only after commit. If admission fails in a batch,
the failing call retains its own error and later unstarted calls report that
they did not run; an uncertain outcome is not retried as a known failure.
There is no per-socket periodic dispatch query during normal observation.
The observer batches up to 1,024 connected targets and uses the
status-leading dispatch index. Its SQL can match cross-pairs; only exact local
subscriptions are notified. Idle sweeps wait 500 ms, so a remote admission
after an idle sweep can take that long to be observed. The index permits a
status-prefix scan without reading completed history; the chosen plan depends
on MatrixOne. Each Server may still scan the global pending backlog and the
database may aggregate cross-pairs.
A stalled observation query wakes connections for bounded-concurrency
individual probes; failed batches also trigger their own recovery probes.
Relay socket sends finish when the write completes, the connection fails, or the
connection is cancelled;
a slow earlier request does not reject untouched requests in the same claimed
batch. The Edge heartbeat can still close a connection that receives no Pong
for its existing timeout while a large unfragmented frame is in progress.
Application Pong and result ACK messages share the existing bounded writer
queue. If it is full, the Server closes the socket. The Edge cancels its running
invocations on disconnect; a result already persisted without an ACK is
replayed on reconnect.
Queued direct work is checked again after acquiring the socket writer: a request
already cancelled or expired at that check is skipped. Cancellation racing with
the check may arrive after sending starts. On one Server, the current connection
generation is checked before starting a relay claim and through the first send
poll. A claim already in progress can race with replacement; replacement on
another Server is still detected through the existing registry heartbeat. Direct request writes
retain their existing five-second timeout; after an uncertain partial write,
cancellation across reconnect remains best effort.
Requests known not to have been sent are
settled together in one conditional update with explicit non-execution results.
An attempted send remains `dispatched` for result replay. Each connection owns
at most one claimed batch; claim queries, settlement owners, and active
settlement queries have separate limits. Returned settlement errors retry for up to 30 seconds; an
in-flight query remains owned until it returns. If ownership or retries are
exhausted, the outcome is unconfirmed and the dispatch is not deliberately
returned to the claimable queue. The
`astra_edge_dispatch_settlement_unknown_total` counter records claimed
unsent dispatches whose durable terminalization could not be confirmed,
including direct delivery and relay settlement. A lost response may mean the
write committed, or another terminal result may have won. Custom dispatch
adapters must implement the direct-settlement reporting hook to include their
direct outcomes. The
`astra_edge_dispatch_observation_unknown_total` counter records dispatch
identities reported to at least one waiter after an unknown observation error
exhausts its retry budget. A Server crash loses the in-memory proof that a claimed
request was never sent; the durable state alone cannot establish that fact.
The number of retained batches is bounded by connection and settlement ownership,
but their bytes are not: outbound requests are materialized whole. Many active
connections with large batches can therefore use substantial memory. The
current Edge clients also retain their WebSocket library's incoming frame and
message limits; accepting a durable request does not guarantee that an
oversized frame can be delivered.
Registry cleanup has a single generation-fenced owner and runs independently
of batch settlement. A closed connection transfers cleanup to separately
bounded capacity before its live-connection slot is released; if that capacity
is exhausted, admission backpressures until an owner finishes. While an Edge's
result write is in progress, its direct result callbacks remain available;
a stalled write can therefore delay them until their own deadlines or
cancellation. Known transient result-observation errors retain waiters until
their deadlines or cancellation. An unclassified database error is retried
for up to 30 seconds per dispatch identity without a successful observation,
then reported as an unknown outcome; a blocked query can delay this check.
Dispatch claims, settlements, and observer queries are awaited instead of
deliberately dropped to start another; a lost connection cannot establish
whether the server stopped the query. Conditional settlement preserves a
terminal winner even when an earlier outcome is unknown. On a panic or unknown query
outcome, the observer rate-limits retries while sending recovery wakes, then
automatically resumes batched observation. Ordinary database errors are
retried. A lost notification subscription closes the Edge connection so it can
reconnect. Pools of one or two connections use recovery probes to preserve
capacity for normal requests. The shared observer and registry heartbeat still
access the database; this is not a claim of zero idle SQL or bounded active
backlog scans. The dispatch index uses the fresh schema; there is no automatic
upgrade of the previous index shape. Cleanup correlates live-run custody through the
invocation identity inside `payload_json` on purpose: the `user_id` column
stores the edge dispatch owner, not the session principal that owns the
`agent_runs` row, so that JSON path is the only carrier of the custody key,
and `agent_runs` has a `(user_id, run_id)` primary key. The actual cleanup
plan depends on MatrixOne's optimizer.

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
generic asynchronous dispatch is cancellation-aware in the executor wrapper.
Bash and run_script terminate and reap their direct processes and attempt to
clean their process group. A process group cannot prove that descendants which
created another session have stopped. Configured CPU or memory limits still
require a working resource boundary before starting a script. A synchronous
non-yielding operation cannot be made preemptible by wrapping it in a Tokio
timeout. A Server-local executor that does not finish bounded cleanup reports
an unknown outcome rather than retaining the waiter and workspace indefinitely;
its old operation may overlap later work.

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
