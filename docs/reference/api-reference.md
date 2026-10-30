# API Reference

Base URL: `http://localhost:17001`

Interactive docs: `http://localhost:17001/docs` (Swagger UI) | `http://localhost:17001/redoc` (ReDoc)

## Authentication

All protected endpoints require a JWT. The public authentication exceptions
are `/live`, `/ready`, `/health`, `/auth/register`, `/auth/login`, `GET /auth/methods`, `POST /auth/memoria`, `/auth/refresh`, and
`/auth/logout` (refresh/logout authenticate the supplied refresh token):

Logout revokes the token's session, including a successor issued by a
concurrent refresh. A signed, unexpired refresh token can terminate its own
session even after rotation; it cannot be used to refresh again. A valid proof
for a session already gone is an idempotent success. Other sessions are unaffected.

```
Authorization: Bearer <access_token>
```

### POST /auth/register

```json
// Request
{"username": "alice", "email": "alice@example.com", "password": "secure_password"}

// Response 201
{"user_id": "uuid", "username": "alice", "email": "alice@example.com"}
```

### POST /auth/login

```json
// Request
{"username": "alice", "password": "secure_password"}

// Response 200
{"access_token": "eyJ...", "refresh_token": "eyJ...", "token_type": "bearer", "expires_in": 3600}
```

### POST /auth/refresh

```json
// Request
{"refresh_token": "eyJ..."}

// Response 200
{"access_token": "eyJ...", "token_type": "bearer", "expires_in": 3600}
```

### GET /auth/me

Returns current user info.

### GET /auth/methods

Returns Server-owned login discovery. With no configured browser integration:

```json
{"password":true,"memoria":null}
```

When enabled, `memoria` contains `issuer` and `authorization_url` (the website base URL). CLI login uses this discovery; it does not assume a hosted website for a self-hosted Server.

### POST /auth/memoria

Accepts `{"connection_key":"<scoped key>"}`. The auth service verifies the key online, then commits the provider-scoped identity, encrypted credential and refresh session in one transaction. Response: `user_id`, `access_token`, `refresh_token`, `token_type`, `expires_in` (at most 900 seconds), `memory_access`, and `granted_scopes`. No Memoria secret is returned.

Invalid keys fail with 401; unavailable verification fails with 503; legacy identity mappings without explicitly configured provenance fail with 409. An identity-only key can log in but cannot access memory.

### DELETE /auth/memoria

Requires an Astra access token. Removes the account's stored Memoria credential and revokes all its Astra refresh sessions in one transaction; returns 204. Existing access tokens then fail session validation. The service operation is idempotent; retrying with an already revoked access token returns 401.

Account identity, Work/history and Memoria memories are retained. This disconnects Astra; it does not delete the Memoria account or revoke keys at their issuer. Upstream key revocation remains owned by Memoria/its integration settings. Ordinary `/auth/logout` signs out only the submitted session and does not disconnect other devices.

---

## Personal subscription connections

All endpoints below require an Astra bearer token. Ownership comes from that
principal, independently of the ChatGPT identity; other owners receive 404.
Renewable provider credentials are never returned.

| Method | Path | Request / result |
| --- | --- | --- |
| POST | `/me/model-connections/chatgpt/authorizations` | `{redirect_uri,label?,connection_id?,retry_authorization_id?}` → `{authorization_id,authorization_url,state,expires_at}` |
| POST | `/me/model-connections/chatgpt/authorizations/{id}/complete` | `{state,code?,client_id?,error?}` → `{connection}` |
| DELETE | `/me/model-connections/chatgpt/authorizations/{id}` | Cancels a pending/exchanging attempt |
| GET | `/me/model-connections` | `{items:[connection]}` metadata only |
| GET | `/me/model-connections/{id}/models` | `{items:[{slug,display_name}]}` visible models only |
| POST | `/me/model-connections/{id}/disconnect` | Clears local credentials, disables linked models; reports `remote_revocation_confirmed` |

Connection metadata includes `connection_id`, `provider`, `label`, optional
`email`, `status`, access-token `expires_at`, and nullable revocation confirmation.
States are `connected`, `plan_disabled`, `reauth_required`, or `disconnected`.
An authorization expires after 15 minutes. Its redirect must be exactly
`http://127.0.0.1:<port>/auth/callback`; Server validates state, PKCE, nonce,
issuer, signature, audience and subject. Start and complete must use the same
Astra owner. Repeating a completed callback is idempotent only for the same
callback values. `connection_id` and `retry_authorization_id` are alternatives.

`POST /me/models` accepts `provider:"chatgpt"` and `connection_id` instead of
`api_key`; omit `base_url`. Use a currently visible provider slug in `model`
and an Astra-local alias in `name`. Creation and `POST /me/models/{id}/check`
run the common Responses probe. API-key and subscription sources are mutually
exclusive. Subscription model credentials cannot be rotated through model
update; reconnect the connection instead.

`GET /models` and `GET /model-access` retain catalog version 2. Each connection
has an opaque access ID `chatgpt:<uuid>`, kind `cloud_byok`, Server placement,
and label `ChatGPT subscription · <label>`. No new public enum or catalog field
is required by released clients. Pricing and reasoning controls are absent.
Missing grants, reauthentication, disconnection and quota failures fail closed;
clients must not refresh their Astra login in response to a provider denial.

## Auto model routing for chat

On `POST /chat` and `POST /chat/stream`, an authenticated Server-catalog user
can opt into the configured economy/strong routing policy:

```json
{"message":"Explain this result", "execution_policy":{"model_routing":"auto"}}
```

Omit `model_selection` for Auto. Missing configuration returns
`model_routing_unavailable`; explicit model state, provider model gateways,
bound Work, or fixed-default semantic admission combined with Auto returns
`model_routing_unsupported`. Existing Offering authorization still applies.
With no Auto request, model selection behaves as before. The initial policy
uses economy only for confidently easy, read-only tasks with compatible model
contracts. See [model routing](../design/model-routing.md#deterministic-auto-selection-stage-3)
for configuration, coverage, and persistence semantics.

## Run history reads

`GET /runs` keeps its compact owner-scoped keyset pagination (`limit`,
`after_updated_at`, `after_run_id`).

`GET /sessions/{session_id}/transcript?scope=conversation&limit=200` returns a
bounded display view for the authenticated Session owner:

```json
{
  "kind": "conversation",
  "session": { "session_id": "...", "metadata": {}, "status": "active" },
  "selected_root": { "run_id": "...", "status": "failed", "waiting_for": null, "created_at": "2026-10-25T03:00:00.123456Z" },
  "items": [{ "kind": "opening", "id": "input:...", "run_id": "...", "source_event_id": "trace:user:...", "created_at": "2026-10-25T03:00:00.123456Z", "content": "exact submitted text" }],
  "history_loaded": true,
  "raw_page": { "next_before_seq": null, "has_more": false },
  "truncated": false
}
```

`session` contains the ordinary Session metadata fields (abbreviated above).
`selected_root` can be terminal or null. `items` contain either an opening input or
`kind: "raw"` with the ordinary transcript fields. Exact committed original-input
rows suppress their matching opening; conflicting identity/content fails the read.
Children and foreign Sessions are excluded before the history window. Input time
is immutable UTC admission time, preserving microseconds. Context authority is
never included.

The view retains up to 200 display items and examines at most 200 historical
opening candidates plus the selected root. Historical opening payload has a
256 KiB budget applied in SQL before body transfer and decoding. It counts both
stored opening JSON bodies plus a conservative display envelope; the most recent
continuous prefix is retained. The selected input is retained exactly under the
existing 8 MiB opening limit. Strings are never truncated. `truncated` signals
older omitted history. Only raw rows have `item_seq`; `raw_page` describes the raw
page independently of composed display items. Continue older raw history through
the existing raw scopes. Conversation scope rejects `run_id` and `before_seq`;
other transcript scopes and cursors are unchanged.

Root status and raw history are read in one database statement snapshot. Session
metadata and artifact previews use their existing separate read boundaries.
Selected detail refreshes perform a fresh bounded read. A client with a reusable
continuation can send `reuse_run_id` and `reuse_terminal=true` while retaining
that continuation until its actual stream terminal. If the snapshot selects that
same Run and it is active (or terminal preservation was requested), the response
has `history_loaded:false`, `items:[]`, `raw_page:null`, and `truncated:null`.
These nulls mean unexamined history, not an empty transcript. A successor returns
full bounded history in the same read. Clients must recheck their continuation
after awaiting the response; an invalidated parser cannot use omitted history to
rebuild a cold view. Continuation parameters are rejected outside conversation scope.

Web metadata mutations commit only their changed fields after backend success
and invalidate overlapping older detail/directory reads. A failed workspace update
does not roll back another successful selection. This fences one Web store's
concurrent publication; it does not promise linearizable caches across Web servers.

The Web `POST /api/chats/{chat_id}/messages` JSON endpoint shares the streaming
message/parser owner. Concurrent observation does not discard its pending input
or result. If its stream is interrupted after Run admission, it returns HTTP 502
with `code: "stream_observation_interrupted"`; the admitted Run and cursor remain
available for stream reconnection. Failures before an admission acknowledgement
remain failed local assistant messages. Stop, Session deletion and a selected successor prevent late
responses from changing the current view.

## Active run permission mode

### POST /chat/runs/{run_id}/permission-mode

The authenticated owner can select a permission policy for an active root run.
The session ID must match the run. Request IDs are idempotent; reusing an ID
with another mode returns 409.

```json
{"expected_session_id":"session-1","request_id":"change-1","mode":"bypass"}
```

Modes use the canonical wire values: `prompt`, `accept_edits`, `plan`, `auto`,
`bypass`, and `deny`. Here `plan` means read-only tool policy; it does not create
or approve a planning workflow.

The response acknowledges durable acceptance:

```json
{"request_id":"change-1","mode":"bypass","revision":12}
```

The run captures the latest accepted policy at its next model-round boundary.
An already-started round keeps its captured server policy. Acceptance alone is
not proof of application. The `permission_mode_applied` stream event reports
`request_id`, `mode`, `revision`, `round_index`, and `owner_generation` after the
runtime has applied the policy and durably recorded the acknowledgement.
Bypass retains capability restrictions and policy hard denies.

Missing, foreign, non-root, or session-mismatched runs return 404. Inactive runs
and request identity conflicts return 409; unavailable storage or an
unconfirmed commit returns 503. Retry an unconfirmed request with the same
request ID.

### GET /chat/runs/{run_id}/permission-mode?expected_session_id={session_id}

Returns a bounded snapshot for recovering a pending acknowledgement:

```json
{
  "requested":{"request_id":"change-1","mode":"bypass","revision":12},
  "applied":{
    "selection":{"request_id":"change-1","mode":"bypass","revision":12},
    "round_index":3,
    "owner_generation":0
  }
}
```

Either field can be `null`. A newer `requested` revision than the `applied`
selection remains pending. Clients can stop polling once the requested revision
is acknowledged; ordinary observation uses the run stream.

---

## Agents

### POST /agents

```json
// Request
{"agent_name": "code-reviewer", "system_prompt": "You review code.", "config": {"model": "gpt-4"}}

// Response 201
{"agent_id": "uuid", "agent_name": "code-reviewer", "owner_user_id": "uuid", "is_active": true, "created_at": "..."}
```

### GET /agents

List all agents owned by the current user. This endpoint does not accept
pagination parameters.

### GET /agents/{agent_id}

### PUT /agents/{agent_id}

### DELETE /agents/{agent_id} → 204

Agent names are unique per owner. Creating a duplicate name or renaming an
agent to another of the owner's names returns 409 without changing either
record. Different owners may use the same name. Reads and mutations of another
owner's agent, and reads or deletes of a missing agent, return 404.

---

## Sessions

### POST /sessions

```json
// Request
{"agent_id": "uuid", "metadata": {"context": "code_review"}}

// Response 201
{"session_id": "uuid", "user_id": "uuid", "status": "active", "event_count": 0}
```

### GET /sessions

Query params: `agent_id`, `session_status`, `limit`, `after_updated_at`, and
`after_session_id`. The two `after_*` values form a seek cursor and must be
provided together; `offset` pagination is not supported.

### GET /sessions/{session_id}

### GET /sessions/{session_id}/transcript

Owner-scoped item pagination accepts `limit`, `before_seq`, and either
`scope=root_conversation` or `run_id`. Combining those filters is rejected;
omitting both selects the session audit stream. Items retain ascending
`item_seq` order and stable `source_event_id` values. The response contains
`session_id`, `items`, `next_before_seq`, and `has_more`. Use the returned
sequence with the same scope for older items. Artifact hydration is included.
Physical page references/hashes are no longer part of this response.

Work transcript responses retain their committed cursor and per-item commitment
fields; a newer canonical context head does not certify transcript publication.

### PUT /sessions/{session_id}

### POST /sessions/{session_id}/close

### DELETE /sessions/{session_id} → 204

---

## Events

### POST /events

```json
// Request
{
  "session_id": "uuid",
  "event_type": "user_query",
  "content": "Review auth.py for security issues",
  "parent_event_id": null,
  "metadata": {"source": "cli"}
}

// Response 201
{
  "event_id": "uuid",
  "session_id": "uuid",
  "event_type": "user_query",
  "content": "...",
  "causal_chain_id": "uuid",
  "parent_event_id": null,
  "created_at": "..."
}
```

### GET /events

Query params: `session_id`, `event_type`, `agent_id`, `causal_chain_id`,
`limit`, `after_created_at`, and `after_event_id`. The two `after_*` values
form a seek cursor and must be provided together; `offset` pagination is not
supported.

Returns event records for list views, including `content` and `metadata`.

> Legacy `/tasks` and `/tasks/{task_id}` routes are not registered by the current
> runtime. They are not public capabilities; use the versioned Work API under
> `/v1/works` for the canonical work/task graph contract.

### GET /events/{event_id}

### GET /events/session/{session_id}

Session-scoped event summaries, ordered by creation time. The `content` field may be truncated for efficiency; use `GET /events/{event_id}` for full event content and metadata.

### GET /events/causal-chain/{chain_id}

All events in a causal chain.

### DELETE /events/{event_id} → 204

---

## Skills

### GET /skills

### GET /skills/{skill_id}

### GET /skills/{skill_id}/versions

### GET /skills/user

### POST /skills/user

### POST /skills/user/{skill_name}/versions

---

## Context Snapshots

### POST /context

Create a context snapshot (records exact LLM input before a call).

### GET /context

Query params: `session_id`, `limit`, `after_created_at`, and
`after_context_capture_id`. The two `after_*` values form a seek cursor and
must be provided together.

### GET /context/{snapshot_id}

---

## Decision Audit

### POST /decisions

Record a decision with link to context snapshot.

### GET /decisions

Query params: `session_id`, `decision_type`, `limit`, `after_created_at`, and
`after_decision_id`. The two `after_*` values form a seek cursor and must be
provided together.

### GET /decisions/{decision_id}

### GET /decisions/{decision_id}/audit

Full audit: decision + linked context snapshot + source events.

---

## Health

### GET /live

Dependency-free process liveness. Returns `200` while the Server process can
serve HTTP, even when an external dependency is unavailable.

```json
{"status": "alive"}
```

### GET /ready

Traffic readiness. Returns `200` when the database is healthy and `503` when
the replica should be removed from service routing.

```json
{"status": "ready", "database": "connected"}
```

### GET /health

Aggregate diagnostic state, including optional Memoria degradation and build
identity. `build_git_dirty=false` is an explicit clean-build declaration;
`true` means tracked source changes **or an unverified source state**. Ordinary
Cargo builds (debug or release) without a valid attestation nonce or a complete
trusted source declaration conservatively report `true`, even in a clean
checkout. They do not watch every tracked file, so unrelated edits can reuse
core without leaving an old clean identity that artifact verification accepts.

Identity-sensitive builds must either use a valid 64-hex
`ASTRA_BUILD_ATTESTATION_NONCE` (which watches all tracked files, even when the
nonce is reused), or a trusted orchestrator must supply both the full
`ASTRA_BUILD_SOURCE_GIT_SHA` and `ASTRA_BUILD_SOURCE_GIT_DIRTY=true|false` for the
exact source being compiled. The orchestrator must refresh that declaration on
every build and prevent source changes during compilation; setting a constant
`false` in a development shell is not verification. Container/manual harness
builds and `scripts/dev/start-api.sh` use the explicit declaration. Missing or
malformed declarations cannot establish a clean identity. The same conservative
boolean is exposed through `--build-info-json`, so existing clean-artifact probes
reject ordinary unverified builds. This endpoint is for observation;
use `/live` and `/ready` for orchestrator probes.

```json
{
  "status": "healthy",
  "database": "connected",
  "memoria": "available",
  "interaction_api_major": "3",
  "build_git_sha": "0123456789abcdef0123456789abcdef01234567",
  "build_git_dirty": false
}
```

## Error Format

```json
{"detail": "Error message"}
```

| Status | Meaning |
|---|---|
| 400 | Invalid request |
| 401 | Missing/invalid JWT |
| 403 | Not authorized (not resource owner) |
| 404 | Resource not found |
| 429 | Rate limited (60 req/min) |
| 500 | Server error |

### Server chat execution authority

`POST /chat` and `POST /chat/stream` own their Server execution. The request
field `conversation_authority` is unsupported and rejected as
`chat_request_invalid`, including when its value is `null`, before Session or
Run admission. A caller must finish or release an outstanding standalone
Controller writer before starting Server chat. Session/Work Controller APIs
and their signed execution grants remain separate supported contracts.

## Retired persistence APIs

`/sandbox` and `/sandbox/{name}` metadata CRUD,
`/data-versioning/sandbox/{name}/checkpoint`, `/marketplace/quality-report`,
`/marketplace/stats/{skill_name}` and `/marketplace/search` are no longer routed.
Ordinary data-versioning checkpoints and skill registry/install APIs remain.

`POST /sessions/{id}/resume` returns canonical hydration (`session_id`,
`last_status`, diagnostic totals and `resume_bundle`), rather than a Session
header with `created_at`. Both full and `?continuation_only=true` restore
require valid owner-scoped canonical Context; missing state returns 404 without
activating the Session. Corrupt canonical state fails without activation.
