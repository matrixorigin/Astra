# API Reference

Base URL: `http://localhost:17001`

Interactive docs: `http://localhost:17001/docs` (Swagger UI) | `http://localhost:17001/redoc` (ReDoc)

## Authentication

All protected endpoints require a JWT. The public authentication exceptions
are `/health`, `/auth/register`, `/auth/login`, `/auth/refresh`, and
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

### GET /health

```json
{"status": "healthy", "database": "connected"}
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
