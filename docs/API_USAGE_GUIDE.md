# API Usage Guide

Complete guide for using the mo-agent-engine REST API.

## Base URL

```
http://localhost:8000
```

## Authentication

All endpoints (except `/health` and `/auth/*`) require JWT authentication.

### 1. Register a User

```bash
curl -X POST http://localhost:8000/auth/register \
  -H "Content-Type: application/json" \
  -d '{
    "username": "alice",
    "email": "alice@example.com",
    "password": "secure_password"
  }'
```

Response:
```json
{
  "user_id": "usr_abc123",
  "username": "alice",
  "email": "alice@example.com",
  "created_at": "2026-02-12T14:30:00Z"
}
```

### 2. Login

```bash
curl -X POST http://localhost:8000/auth/login \
  -H "Content-Type: application/json" \
  -d '{
    "username": "alice",
    "password": "secure_password"
  }'
```

Response:
```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIs...",
  "refresh_token": "eyJhbGciOiJIUzI1NiIs...",
  "token_type": "bearer"
}
```

### 3. Use Token

Include the access token in all subsequent requests:

```bash
curl -X GET http://localhost:8000/agents \
  -H "Authorization: Bearer eyJhbGciOiJIUzI1NiIs..."
```

## Core Workflows

### Workflow 1: Create and Track a Conversation

#### Step 1: Create an Agent

```bash
curl -X POST http://localhost:8000/agents \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Code Review Assistant",
    "agent_config": {
      "model": "gpt-4",
      "temperature": 0.7
    }
  }'
```

Response:
```json
{
  "agent_id": "agent_xyz789",
  "name": "Code Review Assistant",
  "agent_type": "conversational",
  "owner_user_id": "usr_abc123",
  "agent_config": {
    "model": "gpt-4",
    "temperature": 0.7
  },
  "is_active": true,
  "created_at": "2026-02-12T14:35:00Z"
}
```

#### Step 2: Create a Session

```bash
curl -X POST http://localhost:8000/sessions \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "agent_id": "agent_xyz789",
    "title": "Review PR #123",
    "metadata": {
      "pr_number": 123,
      "repository": "my-repo"
    }
  }'
```

Response:
```json
{
  "session_id": "sess_def456",
  "user_id": "usr_abc123",
  "agent_id": "agent_xyz789",
  "title": "Review PR #123",
  "status": "active",
  "event_count": 0,
  "metadata": {
    "pr_number": 123,
    "repository": "my-repo"
  },
  "created_at": "2026-02-12T14:36:00Z"
}
```

#### Step 3: Log Events

```bash
# User query
curl -X POST http://localhost:8000/events \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "session_id": "sess_def456",
    "event_type": "user_query",
    "content": "Please review the changes in this PR"
  }'

# LLM response
curl -X POST http://localhost:8000/events \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "session_id": "sess_def456",
    "event_type": "llm_response",
    "content": "I will review the PR...",
    "agent_id": "agent_xyz789",
    "parent_event_id": "evt_prev123"
  }'
```

### Workflow 2: Auditable Decision Making

#### Step 1: Create Context Snapshot

```bash
curl -X POST http://localhost:8000/context \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "session_id": "sess_def456",
    "event_id": "evt_abc123",
    "context_data": {
      "system_prompt": "You are a code review assistant",
      "skill_definitions": ["code_review", "bug_detection"],
      "selected_events": ["evt_001", "evt_002"],
      "code_context": {
        "file": "main.py",
        "lines": "1-50"
      },
      "total_tokens": 1500,
      "task_type": "code_review"
    }
  }'
```

Response:
```json
{
  "snapshot_id": "snap_ghi789",
  "session_id": "sess_def456",
  "event_id": "evt_abc123",
  "context_data": { ... },
  "created_at": "2026-02-12T14:40:00Z"
}
```

#### Step 2: Record Decision

```bash
curl -X POST http://localhost:8000/decisions \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "session_id": "sess_def456",
    "event_id": "evt_abc123",
    "snapshot_id": "snap_ghi789",
    "decision_type": "skill_selection",
    "decision_output": {
      "selected_skill": "code_review",
      "confidence": 0.95
    },
    "model_params": {
      "model": "gpt-4",
      "temperature": 0.7,
      "max_tokens": 2000
    }
  }'
```

Response:
```json
{
  "decision_id": "dec_jkl012",
  "session_id": "sess_def456",
  "event_id": "evt_abc123",
  "snapshot_id": "snap_ghi789",
  "decision_type": "skill_selection",
  "decision_output": { ... },
  "model_params": { ... },
  "created_at": "2026-02-12T14:41:00Z"
}
```

#### Step 3: Audit Decision (Time-Travel)

```bash
curl -X GET http://localhost:8000/decisions/dec_jkl012/audit \
  -H "Authorization: Bearer $TOKEN"
```

Response includes full context:
```json
{
  "decision_id": "dec_jkl012",
  "decision_type": "skill_selection",
  "decision_output": { ... },
  "model_params": { ... },
  "context": {
    "system_prompt": "You are a code review assistant",
    "skill_definitions": ["code_review", "bug_detection"],
    "selected_events": ["evt_001", "evt_002"],
    "code_context": { ... },
    "total_tokens": 1500
  },
  "created_at": "2026-02-12T14:41:00Z"
}
```

### Workflow 3: Session Replay and Regression Testing

#### Step 1: Replay Session

```bash
curl -X POST http://localhost:8000/sessions/sess_def456/replay \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "mock_mode": true,
    "sandbox_name": "test_sandbox"
  }'
```

Response:
```json
{
  "replay_id": "replay_mno345",
  "session_id": "sess_def456",
  "status": "completed",
  "events_replayed": 5,
  "result": {
    "events": [
      {
        "event_id": "evt_001",
        "event_type": "user_query",
        "success": true,
        "mode": "mock"
      }
    ],
    "total": 5,
    "successful": 5,
    "failed": 0
  },
  "mock_mode": true,
  "created_at": "2026-02-12T14:45:00Z"
}
```

#### Step 2: Compare Results

```bash
curl -X GET http://localhost:8000/sessions/sess_def456/replay/compare \
  -H "Authorization: Bearer $TOKEN"
```

Response:
```json
{
  "session_id": "sess_def456",
  "original_event_count": 5,
  "replay_event_count": 5,
  "difference": 0,
  "match": true,
  "mismatched_events": 0,
  "details": [],
  "compared_at": "2026-02-12T14:46:00Z"
}
```

### Workflow 4: Skill Management

#### Register a Skill

```bash
curl -X POST http://localhost:8000/skills \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "skill_id": "code_review_v2",
    "skill_name": "Code Review",
    "skill_version": "2.0.0",
    "skill_code": "def review_code(code): ...",
    "description": "Advanced code review with security checks",
    "metadata": {
      "category": "code_analysis",
      "requires": ["ast", "pylint"]
    }
  }'
```

#### List Skill Versions

```bash
curl -X GET http://localhost:8000/skills/code_review_v2/versions \
  -H "Authorization: Bearer $TOKEN"
```

Response:
```json
[
  {
    "version": "2.0.0",
    "description": "Advanced code review with security checks",
    "created_at": "2026-02-12T14:50:00Z"
  },
  {
    "version": "1.0.0",
    "description": "Basic code review",
    "created_at": "2026-02-01T10:00:00Z"
  }
]
```

## Error Handling

All errors follow this format:

```json
{
  "detail": "Error message here"
}
```

Common HTTP status codes:
- `200` - Success
- `201` - Created
- `204` - No Content (successful deletion)
- `400` - Bad Request
- `401` - Unauthorized (invalid/missing token)
- `403` - Forbidden (valid token, insufficient permissions)
- `404` - Not Found
- `500` - Internal Server Error

## Rate Limiting

Currently no rate limiting is enforced. In production, consider:
- 60 requests per minute per user
- 1000 requests per hour per user

## Pagination

List endpoints support pagination:

```bash
curl -X GET "http://localhost:8000/events?limit=20&offset=40" \
  -H "Authorization: Bearer $TOKEN"
```

Parameters:
- `limit`: Number of items to return (default: 50, max: 100)
- `offset`: Number of items to skip (default: 0)

## Interactive Documentation

Visit these URLs for interactive API documentation:
- Swagger UI: `http://localhost:8000/docs`
- ReDoc: `http://localhost:8000/redoc`

## SDK Examples

### Python

```python
import requests

# Setup
BASE_URL = "http://localhost:8000"
token = "your_access_token"
headers = {"Authorization": f"Bearer {token}"}

# Create agent
response = requests.post(
    f"{BASE_URL}/agents",
    headers=headers,
    json={
        "name": "My Agent",
        "agent_config": {"model": "gpt-4"}
    }
)
agent = response.json()

# Create session
response = requests.post(
    f"{BASE_URL}/sessions",
    headers=headers,
    json={"agent_id": agent["agent_id"]}
)
session = response.json()

# Log event
response = requests.post(
    f"{BASE_URL}/events",
    headers=headers,
    json={
        "session_id": session["session_id"],
        "event_type": "user_query",
        "content": "Hello"
    }
)
event = response.json()
```

### cURL Script

```bash
#!/bin/bash

BASE_URL="http://localhost:8000"

# Login
TOKEN=$(curl -s -X POST $BASE_URL/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"password"}' \
  | jq -r '.access_token')

# Create agent
AGENT_ID=$(curl -s -X POST $BASE_URL/agents \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"Test Agent","agent_config":{}}' \
  | jq -r '.agent_id')

echo "Created agent: $AGENT_ID"

# Create session
SESSION_ID=$(curl -s -X POST $BASE_URL/sessions \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"agent_id\":\"$AGENT_ID\"}" \
  | jq -r '.session_id')

echo "Created session: $SESSION_ID"
```

## Best Practices

1. **Always use HTTPS in production**
2. **Store tokens securely** - Never commit tokens to version control
3. **Refresh tokens before expiry** - Access tokens expire after 30 minutes
4. **Use mock mode for testing** - Avoid side effects during replay
5. **Create context snapshots** - Enable full decision auditability
6. **Use sandboxes for experiments** - Isolate testing from production data
7. **Implement retry logic** - Handle transient failures gracefully
8. **Monitor rate limits** - Implement backoff strategies

## Troubleshooting

### Token Expired

```json
{"detail": "Token has expired"}
```

Solution: Use refresh token to get new access token:

```bash
curl -X POST http://localhost:8000/auth/refresh \
  -H "Content-Type: application/json" \
  -d '{"refresh_token": "your_refresh_token"}'
```

### Permission Denied

```json
{"detail": "Permission denied for Session sess_123"}
```

Solution: Ensure you're accessing resources you own. Check user_id matches.

### Resource Not Found

```json
{"detail": "Session sess_123 not found"}
```

Solution: Verify the resource ID is correct and the resource hasn't been deleted.
