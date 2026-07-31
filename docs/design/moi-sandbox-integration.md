# MOI Sandbox Integration

> **Design & Code Review Document**
> Describes the changes introduced to support the MOI sandbox scenario.
> File paths are relative to the repository root.

---

## Background

The MOI sandbox is an isolated execution environment running inside a
Kubernetes pod.  Each sandbox pod runs an `astra-edge` process that connects
to `astra-server` via WebSocket and proxies AI agent tool calls (bash,
read_file, …) to local execution inside the pod.

```
User browser
    │
    ▼
moi-backend / moi-core (matrixflow)
    │  POST /chat/stream
    │  Authorization: Bearer moi-user-token-v1.xxx    ← runtime token
    │  X-Astra-External-Provider: moi
    │  X-Astra-External-Action: authorize_request
    ▼
astra-server
    │  WebSocket /edge/ws  ←────────────────────────────────────────────┐
    │                                                                     │
    │  Tool dispatch (in-memory pool → cross-user fallback → DB relay)   │
    │                                                                     │
    └─► astra-edge (running inside sandbox pod)                          │
            │  Authorization: Bearer moi-user-token-v1.xxx  ← edge-registration token
            └───────────────────────────────────────────────────────────►┘
```

`astra-server` already had a **server-to-server** authentication path
(matrixflow calls astra via HTTP headers).  This integration adds an
**edge WebSocket authentication path** and fixes several multi-pod routing
bugs.

---

## Token Types

Two token formats both use `moi-user-token-v1.*` but serve different purposes:

| Type | Issuer | TTL | Purpose | `purpose` field | `edge_agent_id` |
|------|--------|-----|---------|-----------------|-----------------|
| **Runtime token** | moi-catalog | 10 min | matrixflow → astra HTTP API (existing) | `"runtime"` | absent |
| **Edge-registration token** | moi-backend | 30 days | astra-edge → astra-server WebSocket registration (new) | `"edge_registration"` | bound edge id |

The two types are distinguished by `purpose` and `edge_agent_id` in the token
payload.  Edge-registration tokens carry a `jti` for revocation.

---

## Changes Overview

| ID | Type | Title | Key files |
|----|------|-------|-----------|
| F1 | Feature | MOI edge-registration token WebSocket auth | `services/src/auth/mod.rs`, `runtime/src/server/edge/edge_ws_handler.rs` |
| F2 | Feature | Upstream HTTP proxy support | `crates/astra-edge/src/main.rs`, `crates/astra-thin-client/src/client.rs` |
| F3 | Feature | ExternalAuthorizedRequest runtime context routing by token type | `runtime/src/server/provider_runtime_context.rs`, `services/src/auth/mod.rs` |
| F4 | Feature | Edge agent allow_tools schema dynamic injection | `runtime/src/server/server_loop_host.rs`, `runtime/src/server/run/lifecycle/mod.rs` |
| F5 | Feature | Cross-user edge dispatch (sandbox service-account scenario) | `runtime/src/server/tool_edge_transport.rs`, `astra-server-types/src/edge_connection_pool.rs` |
| F6 | Feature | list_catalog / issue_runtime_context by scope | `services/src/auth/external.rs`, `services/src/auth/mod.rs` |
| F7 | Feature | `edge_agent` CapabilityDescriptor field | `services/src/runs.rs` |
| F8 | Feature | Force EdgeWs transport to EdgeBound routing | `runtime/src/server/tool_route_selection.rs` |
| F9 | Feature | Service-to-service edge status endpoint | `runtime/src/server/edge/edge_service_status_handler.rs` |
| F10 | Feature | Local edge-token verification + connect-time revocation check (replaces the retired external-auth callback) | `services/src/auth/mod.rs`, `services/src/auth/external.rs`, `core/src/config.rs` |
| B1 | Bug Fix | Connection pool generation guards against stale cleanup races | `astra-server-types/src/edge_connection_pool.rs` |
| B2 | Bug Fix | DB registration failure rejects WebSocket connection | `runtime/src/server/edge/edge_ws_handler.rs` |
| B3 | Bug Fix | Heartbeat scoped by edge_id to prevent stale connection refresh | `services/src/multi_agent/edge_registry.rs` |
| B4 | Bug Fix | UnconfiguredEdgeRegistryService returns no-op success | `services/src/multi_agent/edge_registry.rs` |
| B5 | Bug Fix | edge_dispatch poll fast path (MatrixOne FOR UPDATE slow query) | `services/src/multi_agent/edge_dispatch.rs` |
| R1 | Refactor | Phase 1.5 reads from resolved principal — eliminates second provider call | `runtime/src/server/edge/edge_ws_handler.rs` |
| R2 | Refactor | Workspace isolation in same-user dispatch hot path | `astra-server-types/src/edge_connection_pool.rs`, `runtime/src/server/tool_edge_transport.rs` |
| R3 | Refactor | Reconnect transfers pending results to new connection | `astra-server-types/src/edge_connection_pool.rs` |
| R4 | Fix | Transactional reconnect: previous connection stays active until DB success, then atomic swap | `runtime/src/server/edge/edge_ws_handler.rs`, `astra-server-types/src/edge_connection_pool.rs` |
| R5 | Fix | Dockerfile.prebuilt: correct COPY paths and .dockerignore negations | `Dockerfile.prebuilt`, `.dockerignore` |
| R6 | Fix | Proxy: NO_PROXY, Proxy-Authorization, CONNECT timeout | `crates/astra-edge/src/main.rs` |

---

## Detailed Descriptions

### F1 — MOI edge-registration token WebSocket auth

> **Superseded in part by F10**: token verification is now local (no
> authorize_request callback); the binding/three-state semantics below are
> unchanged.

**Files:** `services/src/auth/mod.rs`, `runtime/src/server/edge/edge_ws_handler.rs`

#### Problem

`astra-edge` running inside a sandbox pod connects to `astra-server` via
WebSocket `/edge/ws`.  The existing `current_principal()` only decoded
astra-native JWTs and could not validate `moi-user-token-v1` edge-registration
tokens issued by moi-backend.

#### Design

**`auth/mod.rs` — request-aware provider authorization resolves the full principal:**

```rust
// current_principal_for_request() — token prefix handled internally in auth service
if token.starts_with("moi-user-token-v1") {
    return self.principal_from_edge_token(token, actual_request).await;
}
// context-free current_principal() rejects edge-registration tokens
```

`principal_from_edge_token` calls `authorize_request` **once**, extracts both
the user identity and the `edge_agent_id` / `provider_scope_id` binding from
the single response, and stores them in `AuthProviderAuthorizedRequestContext`.
The provider receives the real method, path, route, request id, and body digest,
so it can enforce which HTTP/WebSocket routes accept this long-lived token.

```rust
pub struct AuthProviderAuthorizedRequestContext {
    pub provider_scope_id: String,   // opaque scope key from provider
    pub edge_agent_id: Option<String>, // Some(_) = edge-registration token
    // ...
}
```

**`edge_ws_handler.rs` — Phase 1 + Phase 1.5 in a single round trip:**

Previously the handler made two separate provider calls: one in `current_user()`
and one in `edge_registration_binding()`.  After R1 (see below), Phase 1.5
reads directly from the already-resolved principal — zero extra network calls.

```
Phase 1  : current_principal_for_request(GET /edge/ws) — calls provider once
Phase 1.5: read edge_agent_id from principal.origin (no network call)
           compare against self-reported value; mismatch → AuthError + close
Phase 2  : register into EdgeConnectionPool (with workspace_id)
Phase 2a : write to DB edge registry
Phase 2b : start cross-pod dispatch relay task
```

**Protection:** a token holder cannot impersonate a different edge_agent_id.
A runtime token (no `edge_agent_id`) is rejected on the WebSocket path. Generic
handlers that only use context-free authentication reject edge-registration
tokens, preventing them from becoming general-purpose HTTP credentials.

#### Scope

Existing astra JWTs do not start with `"moi-user-token-v1"` — the original
path is completely unchanged.  `edge_agent_id: None` in `AuthProviderAuthorizedRequestContext`
leaves non-edge provider tokens unaffected.

---

### F2 — Upstream HTTP proxy support

**Files:** `crates/astra-edge/src/main.rs`, `crates/astra-thin-client/src/client.rs`

#### Problem

IDC sandbox pods run in restricted network namespaces where the only egress
path is an HTTP proxy (`HTTP_PROXY` / `http_proxy` env var).  The original
code called `.no_proxy()` in two places, bypassing system proxies:

1. `astra-edge` WebSocket connections (via `connect_async`)
2. `astra-thin-client` streaming and non-streaming HTTP clients

#### Changes

**`astra-edge/src/main.rs`:**

New `connect_via_proxy()` implements HTTP CONNECT tunneling:
1. Selects the first supported non-empty proxy value. `wss://` checks
   `https_proxy` / `HTTPS_PROXY` before `http_proxy` / `HTTP_PROXY`, but skips
   `https://` proxy endpoints (TLS-to-proxy is not implemented) and continues to
   an `http://` fallback; when no supported fallback exists it fails explicitly
   instead of silently bypassing the proxy. `ws://` uses the HTTP pair.
2. Checks `NO_PROXY` / `no_proxy` exclusion list (exact hostname and domain-suffix matching)
3. TCP-connects to the proxy
4. Sends `CONNECT target:port HTTP/1.1` with `Proxy-Authorization: Basic …` when userinfo is present
5. Reads `200 Connection Established`; enforces a 30-second timeout on each step
6. For `wss://`: performs TLS inside the tunnel before WebSocket upgrade

**`astra-thin-client/src/client.rs`:** `.no_proxy()` removed from both
`streaming_http_client` and `ThinClient::new()`.  A source-level guard test
asserts `.no_proxy()` is absent to prevent accidental re-introduction.

**`astra-core/src/net.rs` + `astra-cli` (2026-07-23 follow-up):** the CLI's
auxiliary clients (task service, todos, preferences, team store, durable
bridge, memoria cloud tools) hard-coded `.no_proxy()` under the "internal =
same host" assumption, which broke inside mandatory-egress-proxy sandboxes
(requests to the remote Astra server timed out; symptom: `Skill sources
unavailable`).  New shared policy `astra_core::net::client_builder_for_target
(url)`: loopback targets stay `no_proxy`, remote targets keep reqwest's
env-aware proxy behavior.  All nine call sites now route through it; the
env-mutating proxy regression test and the cloud_sync tests are serialized
(`serial_test`) because proxy env vars are process-global.

#### Scope

No-proxy environments (all applicable proxy variables empty): identical
behaviour to before.
Proxy environments: requests now route through the proxy as intended.

---

### F3 — ExternalAuthorizedRequest runtime context routing by token type

**Files:** `runtime/src/server/provider_runtime_context.rs`, `services/src/auth/mod.rs`

#### Background

`inject_effective_runtime_context_body()` is called on the streaming chat
endpoint to inject runtime context (model gateway, MCP, skills).

The existing code passed the body through unchanged for all
`ExternalAuthorizedRequest` principals because matrixflow server-to-server
calls already embed `capability_descriptors` in the JSON body.

`astra-edge` also produces an `ExternalAuthorizedRequest` principal (via the
edge-registration token) but has no session and no capability descriptors in
its body — it needs to fetch them via the `_by_scope` path.

#### Changes

`AuthPrincipal` gains `is_edge_registration()` which returns `true` when
`AuthProviderAuthorizedRequestContext.edge_agent_id` is `Some`.

Dispatch logic:

```rust
if principal.is_provider_authorized_request() {
    if principal.is_edge_registration() {
        // edge-registration: no session, fetch context via by_scope
        return inject_edge_registration_runtime_context_body(state, principal, body).await;
    }
    // runtime token: body already has descriptors, pass through
    return Ok(body);
}
```

`inject_edge_registration_runtime_context_body`:
- Strips all caller-supplied runtime fields (`runtime_auth`, `capability_descriptors`, etc.)
- Calls `external_runtime_context_by_scope` to fetch provider-issued context
- Rejects a response whose `provider_scope_id` differs from the authorized principal scope
- Injects provider-issued `selected_model`, `runtime_auth`, `capability_descriptors`
- Replaces `allow_tools` with the intersection of the caller request and
  `runtime_scope.allowed_tools`; missing grants therefore fail closed

#### Scope

- matrixflow server-to-server (runtime token): unchanged pass-through
- astra-edge HTTP calls (edge-registration token): new injection path

---

### F4 — Edge agent allow_tools schema dynamic injection

**Files:** `runtime/src/server/server_loop_host.rs`, `runtime/src/server/run/lifecycle/mod.rs`

When `executor_binding.kind = EdgeAgent` (agent-binding mode), the default
host only installs MCP schemas.  MOI callers pass `allow_tools: ["bash", "read_file"]`
but the model never sees the schemas — tool calls fail.

`merge_allowlisted_edge_tool_schemas()` intersects `allow_tools` with the
edge's capability-filtered tool set and injects only the intersection.
No wildcards; callers cannot exceed the capability surface.

Triggered only when `ExecutorBindingKind::EdgeAgent` and `allow_tools` is
non-empty — all other paths are unaffected.

---

### F5 — Cross-user edge dispatch (sandbox service-account scenario)

**Files:** `runtime/src/server/tool_edge_transport.rs`, `astra-server-types/src/edge_connection_pool.rs`

#### Problem

The sandbox `astra-edge` connects with a service-account token whose `sub` is
`external_authorized:moi:svc-xxx`, while the chat request comes from a
workspace user `external_authorized:moi:user-yyy`.

The original `try_edge_websocket()` looked up edges only under the requesting
user's pool entries — the sandbox edge was invisible, causing `Unavailable`.

#### Changes

**`edge_connection_pool.rs`:**

`EdgeConnection` gains `workspace_id: Option<String>` (from
`provider_scope_id` in the edge-registration token).

`find_edge_by_agent_id(edge_agent_id, workspace_id)` scans all connections
and applies fail-closed workspace isolation:
- `request.workspace_id = Some(ws)` → only edges with `edge.workspace_id = Some(ws)`
- `request.workspace_id = None` → only edges with `edge.workspace_id = None`

**`tool_edge_transport.rs`:**

`try_edge_websocket()` dispatch order:

1. `get_user_edges(user_id, workspace_id)` — workspace-filtered same-user lookup
2. `Ok(None)` or empty → `find_edge_by_agent_id(executor_id, workspace_id)` — cross-user fallback
3. Cross-user fallback also fires when the user has other edges but the pinned executor is not among them

The cross-user fallback requires an explicit `selected_executor_id` (pinned);
it is not triggered for unpinned free-selection dispatches.

It also requires an exact, non-empty `workspace_id`. An unscoped request may
resolve a pinned `edge_agent_id` only among the requesting user's connections
or registry rows. The agent ID is a selector, not an authorization credential.

#### Scope

Workspace isolation applies in both the same-user hot path (R2) and the
cross-user fallback — a workspace-B request cannot reach a workspace-A edge
regardless of user ownership.

---

### F6 — list_catalog / issue_runtime_context by scope

**Files:** `services/src/auth/external.rs`, `services/src/auth/mod.rs`

`external_catalog()` and `external_runtime_context()` require a session
handle.  Edge-registration principals have no session.

Two new provider client methods call provider-side `*_by_scope` action
endpoints using `provider_scope_id + external_subject` in place of a session:

```rust
async fn list_catalog_by_scope(provider, scope_id, subject) -> Result<...>
async fn issue_runtime_context_by_scope(provider, scope_id, subject, request) -> Result<...>
```

These are only invoked on the edge-registration token path (F3).  Deployments
without external auth are unaffected (default impl returns `NOT_IMPLEMENTED`).

---

### F7 — `edge_agent` CapabilityDescriptor field

`RuntimeCapabilityDescriptorsRequest` gains an optional `edge_agent` field
populated by moi-core catalog when a sandbox executor is selected.  It carries
`edge_agent_id` and transport type.  `astra-server` reads this in
`binding_resolution.rs` to establish `ExecutorBinding(EdgeAgent)` and route
tool dispatch to the correct edge agent.

---

### F8 — Force EdgeWs transport to EdgeBound routing

In `routing_decision()`, before the workspace kind check:

```rust
if matches!(request.executor.transport, ToolTransportKind::EdgeWs) {
    return ToolExecutionRouteKind::EdgeBound;
}
```

Without this, `ServerSandbox` workspace kind routed to `ServerLocal`, which
has no local adapter in agent-binding mode and failed silently.

---

### F9 — Service-to-service edge status endpoint

`GET /service/edges/status?user_id=xxx` — authenticated with a static shared
secret (`ASTRA_BACKEND_SERVICE_KEY` env var) via `Authorization: Bearer`.
Allows moi-backend to query live edge connection status for any user without
a user session token.  Returns cross-pod-authoritative DB records enriched
with in-memory `connected_secs`.  Responds `503` on DB failure (distinguishable
from "no edges connected").

Authentication uses constant-time comparison (`constant_time_eq`) to prevent
timing attacks; length mismatch is encoded as `(a.len() != b.len()) as u8`
to avoid the truncation bug that affects `(a.len() ^ b.len()) as u8` when
lengths differ by a multiple of 256.

---

### B1 — Connection pool generation guards against stale cleanup races

When an edge reconnects quickly, the new connection registers in the pool
before the old connection's disconnect cleanup fires.  Without a generation
counter, the cleanup would evict the new connection's pool entry.

`register_with_capabilities` returns a monotonically increasing `generation`.
`unregister_if_generation(expected_gen)` only removes the entry when the
generation matches — a stale cleanup for gen-1 leaves the gen-2 connection
intact.

---

### B2 — DB registration failure rejects WebSocket connection

Previously `let _ = edge_registry.register_or_update(…).await` silently
ignored errors.  On failure the edge appeared connected in the in-memory pool
on this pod but was invisible to other pods via DB routing.

Now the DB registration is performed **before** inserting into the pool (R4).
On failure: the pool entry is never created (preserving any existing healthy
connection), an `AuthError` frame is sent, and the WebSocket closes.

**B2 requires B4:** `UnconfiguredEdgeRegistryService` returns no-op success
so single-node deployments without a DB registry are not affected.

---

### B3 — Heartbeat scoped by edge_id

The heartbeat `UPDATE` previously matched only on `(user_id, edge_agent_id)`.
After reconnect, the old connection's heartbeat still matched and kept the
stale DB row alive, misleading other pods into routing to a disconnected edge.

Fixed by adding `AND edge_id = ?` to the heartbeat `UPDATE`.  A stale
connection's heartbeat matches 0 rows → the WS handler treats this as a
disconnect signal.

---

### B4 — UnconfiguredEdgeRegistryService returns no-op success

Write operations (`register_or_update`, `heartbeat`, `unregister`) changed
from `Err(…)` to `Ok(…)` so that single-node deployments without a DB
registry function normally.  `list_by_user` remains `Err` (cross-pod queries
are legitimately unavailable without a DB).

---

### B5 — edge_dispatch poll fast path

MatrixOne's `SELECT … FOR UPDATE` on empty result sets acquires table/page
locks and takes 8–20 seconds.  The dispatch poll loop ran every 2 seconds,
causing severe tool-call latency even with no pending work.

A non-locking `SELECT COUNT(*)` fast path skips the `FOR UPDATE` transaction
when the count is zero.  A new dispatch arriving between COUNT and FOR UPDATE
is delayed at most one 2-second poll cycle — correctness is unaffected.

---

## Refactors Applied During Code Review

### F10 — Local edge-token verification + connect-time revocation check

**Files:** `services/src/auth/mod.rs`, `services/src/auth/external.rs`, `core/src/config.rs`

moi-core #12865 retired the `POST /api/v1/astra/external-auth` callback (and the
whole provider-session surface) in favor of provider HMAC. Astra's edge path
followed:

- `verify_user_token` verifies `moi-user-token-v1.*` tokens **locally** with the
  shared HMAC key (`auth.edge_token_auth.key`, env
  `${ASTRA_EDGE_TOKEN_HMAC_KEY}`), mirroring moi-core `UserTokenSigner.Verify`
  fail-closed rules (iss=moi-backend ⇒ edge_registration; edge_registration ⇒
  edge_agent_id + jti required).
- `principal_from_edge_token` and `edge_registration_binding` no longer call
  the provider over HTTP; claims map directly to the principal / binding.
- **Revocation** (the one thing a self-contained token cannot carry) is checked
  against moi-core `POST /api/v1/astra/edge-tokens/check`
  (`auth.edge_token_auth.check_endpoint`) on **every surface that accepts an
  edge token** — the edge WebSocket connect AND every HTTP request (chat, GET
  /models, ...). Fail-closed on denial or unavailability.

**Revocation surface (updated 2026-07-29).** Revocation is enforced
per-request with a **30-second positive-only cache** keyed by jti
(`check_edge_token_revoked_cached`): a jti that recently passed is trusted for
up to 30s; denials and check-endpoint outages are never cached and always fail
closed. Operational implications:

- Worst-case revocation propagation on astra surfaces is **≤ 30 seconds**
  (plus the moi-backend renewal grace window of ≤ 5 minutes for
  rotated-away previous jtis — see the dual-valid rotation design).
- Catalog load is bounded at ~1 check per active jti per 30s, not one per
  request.

The earlier design traded per-request revocation away entirely (exposure
bounded only by token TTL); that stance was reversed after review — a revoked
30-day token remaining valid on `/chat/stream` was judged unacceptable.

---

### R1 — Phase 1.5 reads from resolved principal (no second provider call)

**Before:** `edge_ws_handler` called `current_user()` (Phase 1) then
`edge_registration_binding(&token)` (Phase 1.5) — two round trips to the
external provider per WebSocket connection, with a consistency window where
the token could be revoked between calls.

**After:** Phase 1 now calls `current_principal_for_request()` with the real
`GET /edge/ws` descriptor. It stores
`edge_agent_id` and `provider_scope_id` in `AuthProviderAuthorizedRequestContext`
(populated during the single `authorize_request` call inside
`principal_from_edge_token`).  Phase 1.5 reads from `principal.origin` —
zero additional network calls.

The token-prefix logic (`moi-user-token-v1`) remains inside the auth service
where it belongs; the WS handler is not aware of token formats and sees only
the typed principal.

---

### R2 — Workspace isolation in same-user dispatch hot path

`get_user_edges(user_id)` previously returned all edges regardless of
workspace.  A workspace-B request with a pinned `executor_id` could select a
workspace-A edge owned by the same user.

`EdgeConnectionInfo` now carries `workspace_id`.  `get_user_edges` takes
`workspace_id: Option<&str>` and applies the same fail-closed isolation as
`find_edge_by_agent_id`.  `get_all_user_edges` (no workspace filter) is
used for status/display queries only.

---

### R3 — Reconnect transfers pending results to new connection

When a reconnecting edge registers with the same `(user_id, edge_agent_id)`,
the new `EdgeConnection` inherits the old connection's `pending_results` Arc
instead of creating an empty one.

Tool calls admitted before the reconnect complete normally: the edge reports
results via the new WebSocket, `deliver_tool_result` finds the same DashMap,
and the waiting `oneshot` receivers complete.  Tool calls whose requests were
in flight over the old WS channel and were never received by the edge will
time out — the edge can recover them via the `pending_requests` reconnect-dedup
mechanism if it reconnects before the timeout.

---

### R4 — Transactional reconnect

**Problem:** ordering the pool insert and the durable DB registration is a
three-way constraint that neither pure ordering satisfies:

1. **No zombie** — a new connection whose DB registration fails must not have
   already evicted a previously healthy connection.
2. **Preserve pending results** — a reconnect must not lose in-flight tool calls
   admitted on the previous connection (their oneshot waiters live in a shared
   pending-result map).
3. **No half-ready selection** — dispatch must never select a connection whose
   forward loop is not yet running (its buffered messages would be lost).

*DB-first* ordering loses (2): a concurrent cleanup of the previous connection
clears the shared pending map during the DB await.  *Pool-first* ordering loses
(3) — the new sender is selectable before its forward loop starts — and, on DB
failure, needs a rollback that cannot recover the original connection under
concurrent multi-reconnect (loses (1)).

**Fix:** a transactional reconnect that never replaces the active connection
until DB registration succeeds, backed by a durable registration lease:

- `begin_reconnect(key)` — does **not** touch the pool.  Any existing connection
  stays active and selectable throughout the DB await.  It marks the key with a
  *reconnect intent* and captures the current pending-result map.
- While the intent is set, `unregister_generation` (previous connection's
  cleanup) removes the entry but **does not clear** the pending map — the
  reconnect will inherit it (commit) or fail it (abort).
- On DB success, `commit_reconnect` **atomically** installs the new connection,
  inheriting the pending map from the still-live previous connection or, if it
  was cleaned up mid-window, from the reservation.  The new sender becomes
  selectable exactly as its forward loop starts.
- On DB failure, `abort_reconnect` leaves any still-active previous connection
  untouched (no zombie, no rollback); if the previous connection was cleaned up
  during the window it fails the preserved waiters rather than leaving them to
  time out.  The reservation's `Drop` clears the intent as a backstop.
- Durable registration uses a compare-and-swap claim with a 120-second expiry.
  The claim remains held until pool commit, serializing this narrow setup window
  across pods and preventing chained rollback through an unpublished
  generation. Claim acquisition does not change active routing metadata: the
  old generation remains published and heartbeatable. Finalization writes the
  new metadata into a non-routable handoff state; pool commit then releases it
  as the active durable generation. The returned lease contains the exact
  published predecessor record, including hostname, worktree, capabilities,
  and workspace metadata. If the socket closes before pool commit, a
  claim-fenced rollback restores that predecessor. Expiry prevents a crashed
  pod from fencing reconnects forever, and an expired unpublished generation is
  never used as a rollback target.
- `edge_auth_ok` is flushed before the sender is published in the connection
  pool. This preserves the client protocol invariant that authentication is the
  first server application frame; a concurrent ToolRequest cannot overtake it.
- While waiting for the reconnect guard and durable claim, the handler keeps
  consuming WebSocket control frames. Ping/Pong are handled and Close/EOF/error
  abort setup; all immediately queued frames are drained before commit. A claim
  made for a socket that died during the DB await is rolled back instead of
  publishing a dead pool entry.

This satisfies all three constraints. The in-memory pool does not need rollback;
the durable registry still does, and its generation-fenced lease makes that
rollback safe across pods and overlapping reconnects.

---

### R5 — Dockerfile.prebuilt: correct COPY paths and .dockerignore

`COPY rust/target/…` referenced a `rust/` subdirectory that does not exist
(sources live in `crates/`).  The `.dockerignore` also excluded `target/` and
`rust/`, making the build context empty.

Fixed: `COPY target/x86_64-unknown-linux-gnu/release/astra-server …`, and
`.dockerignore` negations re-include the two prebuilt release binaries.

---

### R6 — Proxy: NO_PROXY, Proxy-Authorization, CONNECT timeout

Four gaps in the original `connect_via_proxy`:

1. **NO_PROXY ignored:** target host is now checked against the first non-empty
   `no_proxy` / `NO_PROXY` value (comma-separated, exact and domain-suffix
   matching) before using the proxy.
2. **Proxy-Authorization missing:** userinfo (`user:pass@`) was stripped for
   log sanitisation but never sent.  Now emits `Proxy-Authorization: Basic …`
   when userinfo is present.
3. **No timeout:** TCP connect, CONNECT write, response read, and TLS+WS upgrade
   now each have a 30-second timeout.
4. **Empty lowercase variables shadowed uppercase values:** selection now skips
   empty or unsupported values. Secure WebSockets inspect the HTTPS proxy pair
   first, then fall back to an `http://` proxy because TLS-to-proxy is not yet
   implemented.

---

## Architecture Notes and Known Limitations

### Architectural debt (not fixed in this PR)

| ID | Description | Planned fix |
|----|-------------|-------------|
| AD-1 | `token.starts_with("moi-user-token-v1")` in the generic auth service is MOI-specific policy. | Introduce a `TokenClassifier` abstraction that maps token prefixes to handler strategies; auth service wires classifiers at construction time. |
| AD-2 | `provider_scope_id` is used as `workspace_id` under an implicit 1:1 mapping assumption (true for MOI, not guaranteed for other providers). See doc comment on `AuthProviderAuthorizedRequestContext`. | Explicit `workspace_id` field returned by `authorize_request`; `provider_scope_id` kept as the raw opaque key. |
| AD-3 | Edge dispatch is best-effort: no durable admission record, no result ACK, no exactly-once guarantee.  "DB route owner" and "logical invocation owner" are not formally separated. | Durable dispatch table with explicit `admitted`/`delivered` state machine; separate PR/design. |
| AD-4 | `edge_registration_binding` remains the production compatibility fallback for provider principals that do not carry the R1 authorization context. The WebSocket path still calls it in that case. | Migrate all providers to explicit authorization context first; remove the fallback only after usage telemetry and compatibility review. |

### Open gaps (require cross-repo coordination)

| ID | Description | Risk |
|----|-------------|------|
| GAP-1 | Edge-registration token TTL is 30 days; astra cannot proactively revoke tokens (jti blocklist requires matrixflow-side implementation). | 🟡 Medium |
| GAP-2 | `_by_scope` action endpoints (`list_catalog_by_scope`, `issue_runtime_context_by_scope`) must be implemented on the matrixflow side; without them astra-edge HTTP runtime context calls return `501`. | 🔴 Blocking |
| GAP-3 | `edge_agent` CapabilityDescriptor (F7) must be populated by moi-core catalog when issuing runtime context; this PR only consumes the field. | 🟡 Medium |

### Pre-merge requirements

1. **B2 + B4 must land together** — B2 alone rejects all edge connections in
   deployments without a DB registry.
2. **GAP-2 `_by_scope` actions** must be implemented on the matrixflow side
   before astra-edge can fetch runtime context via HTTP.
3. **GAP-3 `edge_agent` descriptor** must be populated by moi-core catalog.

---

## Test Coverage

| Location | Coverage |
|----------|---------|
| `services/src/auth/mod.rs` | Edge token authorization forwards the actual request descriptor; context-free authentication rejects edge tokens |
| `services/src/auth/external.rs` | Runtime context whose scope differs from the authorized principal is rejected |
| `runtime/src/server/provider_runtime_context.rs` | Effective `allow_tools` is the caller/provider intersection and missing grants fail closed |
| `runtime/tests/edge_ws_e2e.rs` | Phase 1.5 binding check: mismatched self-reported `edge_agent_id` rejected; workspace-scoped and cross-user dispatch; DB registration failure rolls back cleanly |
| `astra-server-types/src/edge_connection_pool.rs` | Same-user/two-workspaces isolation; `get_user_edges` workspace filter; `get_all_user_edges` spans workspaces; reconnect pending-result transfer; generation cleanup races |
| `services/src/multi_agent/edge_registry.rs` | `UnconfiguredEdgeRegistryService` no-op behaviour |
| `runtime/src/server/tool_route_selection.rs` | EdgeWs transport routes to EdgeBound under `ServerSandbox` and `EdgeWorkspace` |
| `runtime/tests/system_matrix_http_e2e/journey_saas_negative_matrix.rs` | `/service/edges/status`: missing key → 401, invalid key → 401, valid key → 200 |
| `astra-thin-client/src/client.rs` | Source-level guard: `.no_proxy()` absent from `streaming_http_client` |
