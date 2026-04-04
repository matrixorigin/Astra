# API Reference

Base URL: `http://localhost:8000`

Interactive docs: `http://localhost:8000/docs` (Swagger UI) | `http://localhost:8000/redoc` (ReDoc)

## Authentication

All endpoints except `/health`, `/auth/register`, `/auth/login` require JWT:

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

List agents owned by current user. Query params: `limit`, `offset`.

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

Query params: `limit`, `offset`.

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

Query params: `session_id`, `limit`, `offset`.

Returns event summaries for list views. The `content` field may be truncated for efficiency; use `GET /events/{event_id}` for full event content and metadata.

### GET /tasks

Query params: `status`.

Returns lightweight task summaries only (`task_id`, `title`, `status`, progress counters, timestamps, `project_type`). Use `GET /tasks/{task_id}` for full task detail including description, plan, checkpoint, and error fields.

### GET /tasks/{task_id}

Returns the full task record, including long-text and JSON fields.

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

List sandboxes. Query params: `status`, `created_by`.

### GET /sandbox/{name}

### DELETE /sandbox/{name} → 204

---

## Replay

### POST /sessions/{session_id}/replay

Replay a session. Events are re-executed with tool mocking (no real side effects).

### GET /sessions/{session_id}/replay/compare

Compare original session with replay results.

---

## Skills

### POST /skills

```json
// Request
{
  "skill_name": "code_review",
  "version": "1.0.0",
  "definition": {"description": "Reviews code", "parameters": {...}},
  "side_effect_profile": {"category": "read", "idempotent": true}
}

// Response 201
{"skill_id": "uuid", "skill_name": "code_review", "version": "1.0.0"}
```

### GET /skills

### GET /skills/{skill_id}

### GET /skills/{skill_id}/versions

---

## Context Snapshots

### POST /context

Create a context snapshot (records exact LLM input before a call).

### GET /context

### GET /context/{snapshot_id}

---

## Decision Audit

### POST /decisions

Record a decision with link to context snapshot.

### GET /decisions

### GET /decisions/{decision_id}

### GET /decisions/{decision_id}/audit

Full audit: decision + linked context snapshot + source events.

---

## Health

### GET /health

```json
{"status": "healthy", "database": "connected"}
```

---

## Learning

### POST /api/v1/learning/trigger

Trigger learning cycle from recent failures.

```json
// Request
{
  "days": 7,
  "force": false,
  "signal_types": ["wrong_skill", "slow_execution", "high_cost", "low_satisfaction"],
  "weights": {
    "accuracy": 0.4,
    "speed": 0.3,
    "cost": 0.2,
    "satisfaction": 0.1
  }
}

// Response 200
{
  "status": "success",
  "learned": 15,
  "signals_by_type": {
    "wrong_skill": 8,
    "slow_execution": 5,
    "high_cost": 2
  },
  "gate_verdict": "pass",
  "improvement_pct": 12.5,
  "test_count": 20,
  "model_version": "v1.0"
}
```

### GET /api/v1/learning/signals

Get available learning signal types.

```json
// Response 200
{
  "signal_types": ["wrong_skill", "slow_execution", "high_cost", "low_satisfaction"],
  "descriptions": {
    "wrong_skill": "Incorrect skill selection",
    "slow_execution": "Execution time exceeds threshold",
    "high_cost": "Execution cost exceeds budget",
    "low_satisfaction": "User satisfaction below threshold"
  }
}
```

### GET /api/v1/learning/stats

Get comprehensive learning statistics.

```json
// Response 200
{
  "total_learnings": 25,
  "high_confidence": 15,
  "low_confidence": 10,
  "avg_confidence": 65.5,
  "by_signal_type": {
    "wrong_skill": 10,
    "slow_execution": 8,
    "high_cost": 5,
    "low_satisfaction": 2
  },
  "weights": {
    "accuracy": 0.4,
    "speed": 0.3,
    "cost": 0.2,
    "satisfaction": 0.1
  },
  "weights_per_signal": {
    "slow_execution": {
      "accuracy": 0.3,
      "speed": 0.5,
      "cost": 0.1,
      "satisfaction": 0.1
    }
  },
  "decay": {
    "enabled": false,
    "half_life_days": 0.0,
    "min_confidence": 0.0,
    "per_signal": {}
  },
  "total_gates": 5,
  "passed_gates": 4,
  "failed_gates": 1,
  "pass_rate": 0.8,
  "avg_improvement_pct": 12.5,
  "last_learning_time": "2026-02-14T10:30:00Z"
}
```

### POST /api/v1/learning/feedback

Submit feedback for a skill selection event.

```json
// Request
{
  "event_id": "evt_123",
  "feedback_type": "wrong_skill",
  "correct_skills": ["github_create_pr"],
  "satisfaction_score": 4,
  "comment": "Better skill selection needed"
}

// Response 200
{
  "status": "success",
  "message": "Feedback recorded for event evt_123"
}
```

### GET /api/v1/learning/health

Health check for learning service.

```json
// Response 200
{
  "status": "healthy",
  "service": "learning",
  "version": "1.0.0",
  "timestamp": "2026-02-14T23:00:00Z"
}
```

---

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
