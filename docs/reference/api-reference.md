# API Reference

Base URL: `http://localhost:17001`

Interactive docs: `http://localhost:17001/docs` (Swagger UI) | `http://localhost:17001/redoc` (ReDoc)

## Authentication

All protected endpoints require a JWT. The public authentication exceptions
are `/live`, `/ready`, `/health`, `/auth/register`, `/auth/login`, `GET /auth/methods`, `POST /auth/memoria`, `/auth/refresh`, and
`/auth/logout` (refresh/logout authenticate the supplied refresh token):

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

## Sandbox

### POST /sandbox

```json
// Request
{"name": "experiment_1", "description": "Test new prompt", "created_by": "alice"}

// Response 201
{"sandbox_name": "experiment_1", "status": "active"}
```

### GET /sandbox

List the current user's sandboxes. Query param: optional `pattern` name
filter.

### GET /sandbox/{name}

### DELETE /sandbox/{name} → 204

---

## Replay

### POST /sessions/{session_id}/replay

Reserved for durable replay reconstruction. For an owned session this route
currently returns HTTP 501 with an explicit unavailable detail; it does not
create a replay identity or execute any provider, tool, or external call.
Missing or foreign sessions return HTTP 404 without revealing ownership.

### GET /sessions/{session_id}/replay/compare

Reserved for durable replay reconstruction. For an owned session this route
currently returns HTTP 501 with an explicit unavailable detail; it does not
return replay or comparison counts. Missing or foreign sessions return HTTP
404 without revealing ownership.

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
identity. `build_git_dirty` is `true` when the binary was built from tracked
source changes; local launchers must not reuse such a process after a checkout
change. This endpoint is for observation; use `/live` and `/ready` for
orchestrator probes.

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
