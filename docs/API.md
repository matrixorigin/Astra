# Agent Engine API Documentation

Complete REST API for agent state management, authentication, and conversation tracking.

## Base URL

```
http://localhost:8000
```

## Authentication

All endpoints (except `/auth/register` and `/auth/login`) require JWT authentication.

Include the access token in the `Authorization` header:

```
Authorization: Bearer <access_token>
```

## API Endpoints

### Authentication

#### Register User
```http
POST /auth/register
Content-Type: application/json

{
  "username": "alice",
  "email": "alice@example.com",
  "password": "secure_password",
  "display_name": "Alice"
}

Response: 201 Created
{
  "user_id": "uuid",
  "username": "alice",
  "email": "alice@example.com",
  "display_name": "Alice"
}
```

#### Login
```http
POST /auth/login
Content-Type: application/json

{
  "username": "alice",
  "password": "secure_password"
}

Response: 200 OK
{
  "access_token": "eyJ...",
  "refresh_token": "eyJ...",
  "token_type": "bearer",
  "expires_in": 3600
}
```

#### Refresh Token
```http
POST /auth/refresh
Content-Type: application/json

{
  "refresh_token": "eyJ..."
}

Response: 200 OK
{
  "access_token": "eyJ...",
  "token_type": "bearer",
  "expires_in": 3600
}
```

#### Logout
```http
POST /auth/logout
Authorization: Bearer <access_token>
Content-Type: application/json

{
  "refresh_token": "eyJ..."
}

Response: 200 OK
{
  "message": "Logged out successfully"
}
```

### Agents

#### Create Agent
```http
POST /agents
Authorization: Bearer <access_token>
Content-Type: application/json

{
  "agent_name": "My Assistant",
  "agent_type": "chatbot",
  "config": {
    "model": "gpt-4",
    "temperature": 0.7
  }
}

Response: 201 Created
{
  "agent_id": "uuid",
  "agent_name": "My Assistant",
  "agent_type": "chatbot",
  "owner_user_id": "uuid",
  "config": {...},
  "is_active": true,
  "created_at": "2026-02-12T15:00:00Z"
}
```

#### List Agents
```http
GET /agents
Authorization: Bearer <access_token>

Response: 200 OK
{
  "agents": [
    {
      "agent_id": "uuid",
      "agent_name": "My Assistant",
      "agent_type": "chatbot",
      "owner_user_id": "uuid",
      "config": {...},
      "is_active": true,
      "created_at": "2026-02-12T15:00:00Z"
    }
  ],
  "total": 1
}
```

#### Get Agent
```http
GET /agents/{agent_id}
Authorization: Bearer <access_token>

Response: 200 OK
{
  "agent_id": "uuid",
  "agent_name": "My Assistant",
  ...
}
```

#### Update Agent
```http
PUT /agents/{agent_id}
Authorization: Bearer <access_token>
Content-Type: application/json

{
  "agent_name": "Updated Name",
  "config": {...}
}

Response: 200 OK
{
  "agent_id": "uuid",
  "agent_name": "Updated Name",
  ...
}
```

#### Delete Agent
```http
DELETE /agents/{agent_id}
Authorization: Bearer <access_token>

Response: 204 No Content
```

### Sessions

#### Create Session
```http
POST /sessions
Authorization: Bearer <access_token>
Content-Type: application/json

{
  "metadata": {
    "context": "customer_support"
  }
}

Response: 201 Created
{
  "session_id": "uuid",
  "user_id": "uuid",
  "status": "active",
  "event_count": 0,
  "created_at": "2026-02-12T15:00:00Z",
  "last_active_at": "2026-02-12T15:00:00Z",
  "metadata": {...}
}
```

#### List Sessions
```http
GET /sessions?limit=50&offset=0
Authorization: Bearer <access_token>

Response: 200 OK
{
  "sessions": [
    {
      "session_id": "uuid",
      "user_id": "uuid",
      "status": "active",
      "event_count": 5,
      "created_at": "2026-02-12T15:00:00Z",
      "last_active_at": "2026-02-12T15:05:00Z",
      "metadata": {...}
    }
  ],
  "total": 1
}
```

#### Get Session
```http
GET /sessions/{session_id}
Authorization: Bearer <access_token>

Response: 200 OK
{
  "session_id": "uuid",
  "user_id": "uuid",
  "status": "active",
  ...
}
```

#### Close Session
```http
DELETE /sessions/{session_id}
Authorization: Bearer <access_token>

Response: 204 No Content
```

### Events

#### Create Event
```http
POST /events
Authorization: Bearer <access_token>
Content-Type: application/json

{
  "session_id": "uuid",
  "event_type": "user_query",
  "content": "What is the weather today?",
  "metadata": {
    "source": "web_ui"
  }
}

Response: 201 Created
{
  "event_id": "uuid",
  "session_id": "uuid",
  "user_id": "uuid",
  "event_type": "user_query",
  "content": "What is the weather today?",
  "created_at": "2026-02-12T15:00:00Z",
  "metadata": {...},
  "parent_event_id": null,
  "causal_chain_id": "uuid"
}
```

#### List Events
```http
GET /events?session_id={session_id}&limit=100
Authorization: Bearer <access_token>

Response: 200 OK
{
  "events": [
    {
      "event_id": "uuid",
      "session_id": "uuid",
      "user_id": "uuid",
      "event_type": "user_query",
      "content": "What is the weather today?",
      "created_at": "2026-02-12T15:00:00Z",
      "metadata": {...},
      "parent_event_id": null,
      "causal_chain_id": "uuid"
    }
  ],
  "total": 1
}
```

#### Get Event
```http
GET /events/{event_id}
Authorization: Bearer <access_token>

Response: 200 OK
{
  "event_id": "uuid",
  "session_id": "uuid",
  ...
}
```

### Health Check

```http
GET /health

Response: 200 OK
{
  "status": "healthy",
  "database": "connected"
}
```

## Error Responses

All endpoints return standard HTTP status codes:

- `200 OK` - Success
- `201 Created` - Resource created
- `204 No Content` - Success with no response body
- `400 Bad Request` - Invalid request
- `401 Unauthorized` - Missing or invalid authentication
- `403 Forbidden` - Not authorized to access resource
- `404 Not Found` - Resource not found
- `500 Internal Server Error` - Server error

Error response format:
```json
{
  "detail": "Error message"
}
```

## Event Types

Supported event types:
- `user_query` - User input/question
- `llm_response` - LLM generated response

## Rate Limiting

Currently no rate limiting. Will be added in production.

## Interactive Documentation

Visit `http://localhost:8000/docs` for interactive Swagger UI documentation.
