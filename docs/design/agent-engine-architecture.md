# Agent Engine Architecture Design

## Overview

**mo-agent-engine** is a universal agent state management platform. Any agent (not limited to our platform) can delegate all state management to this engine.

## Core Principles

1. **Service-First**: All access through HTTP API (FastAPI)
2. **Standard Auth**: OAuth2 with access token + refresh token
3. **Database Agnostic**: MatrixOne backend, but tenant is configurable
4. **Universal**: Any agent can use this engine
5. **Multi-Client**: CLI, Web UI, SDK all consume the same API

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Client Layer                                               │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │   Web UI     │  │     CLI      │  │   Python SDK │     │
│  │  (React)     │  │  (mo-agent)  │  │              │     │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘     │
│         │                  │                  │             │
│         └──────────────────┼──────────────────┘             │
│                            │ HTTP + Bearer Token            │
└────────────────────────────┼──────────────────────────────┘
                             │
┌────────────────────────────▼──────────────────────────────┐
│  API Layer (FastAPI)                                      │
│  ┌────────────────────────────────────────────────────┐   │
│  │  Authentication Middleware                         │   │
│  │  - Validate access token (JWT)                     │   │
│  │  - Extract user context                            │   │
│  │  - Rate limiting                                   │   │
│  └────────────────────────────────────────────────────┘   │
│                                                            │
│  ┌────────────────────────────────────────────────────┐   │
│  │  API Endpoints                                     │   │
│  │  - POST /auth/register                             │   │
│  │  - POST /auth/login                                │   │
│  │  - POST /auth/refresh                              │   │
│  │  - POST /chat/completions                          │   │
│  │  - GET  /events                                    │   │
│  │  - POST /skills/execute                            │   │
│  │  - GET  /context/snapshots                         │   │
│  │  - ...                                             │   │
│  └────────────────────────────────────────────────────┘   │
└────────────────────────────┬──────────────────────────────┘
                             │
┌────────────────────────────▼──────────────────────────────┐
│  Business Logic Layer                                     │
│  ┌────────────────────────────────────────────────────┐   │
│  │  Agent Engine Core                                 │   │
│  │  - Event management                                │   │
│  │  - Context management                              │   │
│  │  - Skill execution                                 │   │
│  │  - Memory management                               │   │
│  │  - State persistence                               │   │
│  └────────────────────────────────────────────────────┘   │
└────────────────────────────┬──────────────────────────────┘
                             │
┌────────────────────────────▼──────────────────────────────┐
│  Data Layer                                               │
│  ┌────────────────────────────────────────────────────┐   │
│  │  MatrixOne Database                                │   │
│  │  - Tenant: Configurable via .env                   │   │
│  │  - Tables: users, events, skills, context, etc.    │   │
│  └────────────────────────────────────────────────────┘   │
└───────────────────────────────────────────────────────────┘
```

## Multi-Tenancy Model

### Tenant Configuration

```bash
# .env
DATABASE_HOST=localhost
DATABASE_PORT=6001
DATABASE_USER=app_user
DATABASE_PASSWORD=secret
DATABASE_TENANT=agent_platform  # Can be any tenant, not necessarily 'sys'
DATABASE_NAME=agent_engine
```

### Tenant Structure

```
MatrixOne Cluster
  │
  ├─ agent_platform (Platform Tenant - Configurable)
  │   ├─ agent_engine (Main Database)
  │   │   ├─ users (all users)
  │   │   ├─ agents (agent registry)
  │   │   ├─ events (all events)
  │   │   ├─ skills (skill library)
  │   │   ├─ context_snapshots
  │   │   ├─ sessions
  │   │   └─ ...
  │   │
  │   └─ agent_experiments (Sandbox)
  │       ├─ exp_alice_v1
  │       └─ exp_bob_v2
  │
  ├─ customer_a (Customer A's Tenant - Optional)
  │   └─ agent_engine (Isolated instance)
  │
  └─ customer_b (Customer B's Tenant - Optional)
      └─ agent_engine (Isolated instance)
```

**Key Points**:
- Platform tenant is **configurable** (not hardcoded to 'sys')
- Each customer can have **isolated tenant** for data sovereignty
- Within tenant, use **database-level isolation** for experiments
- Service doesn't care about tenant name, reads from .env

## Authentication & Authorization

### OAuth2 Flow

```
┌─────────┐                                  ┌─────────┐
│ Client  │                                  │ Service │
└────┬────┘                                  └────┬────┘
     │                                            │
     │ 1. POST /auth/register                    │
     │    {username, email, password}            │
     ├──────────────────────────────────────────>│
     │                                            │
     │ 2. User created                            │
     │<───────────────────────────────────────────┤
     │                                            │
     │ 3. POST /auth/login                        │
     │    {username, password}                    │
     ├──────────────────────────────────────────>│
     │                                            │
     │ 4. {access_token, refresh_token}           │
     │    access_token: JWT, expires in 1 hour    │
     │    refresh_token: JWT, expires in 7 days   │
     │<───────────────────────────────────────────┤
     │                                            │
     │ 5. GET /chat/completions                   │
     │    Authorization: Bearer {access_token}    │
     ├──────────────────────────────────────────>│
     │                                            │
     │ 6. Response                                │
     │<───────────────────────────────────────────┤
     │                                            │
     │ 7. Access token expired (401)              │
     │<───────────────────────────────────────────┤
     │                                            │
     │ 8. POST /auth/refresh                      │
     │    {refresh_token}                         │
     ├──────────────────────────────────────────>│
     │                                            │
     │ 9. {access_token, refresh_token}           │
     │    (new tokens)                            │
     │<───────────────────────────────────────────┤
```

### Token Structure

**Access Token** (JWT):
```json
{
  "sub": "user_id_123",
  "username": "alice",
  "email": "alice@example.com",
  "roles": ["user"],
  "exp": 1234567890,
  "iat": 1234564290,
  "type": "access"
}
```

**Refresh Token** (JWT):
```json
{
  "sub": "user_id_123",
  "exp": 1235172090,
  "iat": 1234564290,
  "type": "refresh",
  "jti": "unique_token_id"
}
```

## Database Schema

### Core Tables

```sql
-- Users
CREATE TABLE users (
  user_id           VARCHAR(64) PRIMARY KEY,
  username          VARCHAR(255) UNIQUE NOT NULL,
  email             VARCHAR(255) UNIQUE NOT NULL,
  password_hash     VARCHAR(255) NOT NULL,
  display_name      VARCHAR(255),
  is_active         BOOLEAN DEFAULT TRUE,
  is_verified       BOOLEAN DEFAULT FALSE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  INDEX idx_username (username),
  INDEX idx_email (email)
);

-- Agents (any agent can register)
CREATE TABLE agents (
  agent_id          VARCHAR(64) PRIMARY KEY,
  agent_name        VARCHAR(255) NOT NULL,
  agent_type        VARCHAR(64),              -- 'chatbot' | 'assistant' | 'workflow' | 'custom'
  owner_user_id     VARCHAR(64) NOT NULL,
  config            JSON,                     -- Agent-specific configuration
  is_active         BOOLEAN DEFAULT TRUE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_owner (owner_user_id),
  INDEX idx_type (agent_type)
);

-- Events (all agent events)
CREATE TABLE events (
  event_id          VARCHAR(64) PRIMARY KEY,
  agent_id          VARCHAR(64) NOT NULL,
  user_id           VARCHAR(64) NOT NULL,
  session_id        VARCHAR(64) NOT NULL,
  event_type        VARCHAR(32) NOT NULL,     -- 'user_query' | 'agent_response' | 'skill_execution'
  content           TEXT,
  metadata          JSON,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_agent (agent_id, created_at),
  INDEX idx_session (session_id, created_at),
  INDEX idx_user (user_id, created_at)
);

-- Sessions
CREATE TABLE sessions (
  session_id        VARCHAR(64) PRIMARY KEY,
  agent_id          VARCHAR(64) NOT NULL,
  user_id           VARCHAR(64) NOT NULL,
  status            VARCHAR(32) DEFAULT 'active',
  metadata          JSON,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  
  INDEX idx_agent (agent_id),
  INDEX idx_user (user_id)
);

-- Skills
CREATE TABLE skills (
  skill_id          VARCHAR(64) PRIMARY KEY,
  skill_name        VARCHAR(255) NOT NULL,
  owner_user_id     VARCHAR(64),              -- NULL = platform skill
  skill_type        VARCHAR(64),
  definition        JSON NOT NULL,
  is_active         BOOLEAN DEFAULT TRUE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_owner (owner_user_id),
  INDEX idx_type (skill_type)
);

-- Context Snapshots
CREATE TABLE context_snapshots (
  snapshot_id       VARCHAR(64) PRIMARY KEY,
  session_id        VARCHAR(64) NOT NULL,
  agent_id          VARCHAR(64) NOT NULL,
  snapshot_data     JSON NOT NULL,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_session (session_id, created_at),
  INDEX idx_agent (agent_id, created_at)
);

-- Refresh Tokens (for token revocation)
CREATE TABLE refresh_tokens (
  token_id          VARCHAR(64) PRIMARY KEY,
  user_id           VARCHAR(64) NOT NULL,
  token_hash        VARCHAR(255) NOT NULL,
  expires_at        TIMESTAMP NOT NULL,
  is_revoked        BOOLEAN DEFAULT FALSE,
  created_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  
  INDEX idx_user (user_id),
  INDEX idx_expires (expires_at)
);
```

## API Endpoints

### Authentication

```python
# POST /auth/register
{
  "username": "alice",
  "email": "alice@example.com",
  "password": "secure_password"
}
→ 201 Created
{
  "user_id": "user_123",
  "username": "alice",
  "email": "alice@example.com"
}

# POST /auth/login
{
  "username": "alice",
  "password": "secure_password"
}
→ 200 OK
{
  "access_token": "eyJhbGc...",
  "refresh_token": "eyJhbGc...",
  "token_type": "bearer",
  "expires_in": 3600
}

# POST /auth/refresh
{
  "refresh_token": "eyJhbGc..."
}
→ 200 OK
{
  "access_token": "eyJhbGc...",
  "refresh_token": "eyJhbGc...",
  "token_type": "bearer",
  "expires_in": 3600
}

# POST /auth/logout
Authorization: Bearer {access_token}
{
  "refresh_token": "eyJhbGc..."
}
→ 200 OK
```

### Agent Management

```python
# POST /agents
Authorization: Bearer {access_token}
{
  "agent_name": "My Assistant",
  "agent_type": "chatbot",
  "config": {
    "model": "gpt-4",
    "temperature": 0.7
  }
}
→ 201 Created
{
  "agent_id": "agent_123",
  "agent_name": "My Assistant",
  "owner_user_id": "user_123"
}

# GET /agents
Authorization: Bearer {access_token}
→ 200 OK
[
  {
    "agent_id": "agent_123",
    "agent_name": "My Assistant",
    "agent_type": "chatbot",
    "is_active": true
  }
]
```

### Chat / Completions

```python
# POST /chat/completions
Authorization: Bearer {access_token}
{
  "agent_id": "agent_123",
  "session_id": "session_456",  # Optional, auto-create if not provided
  "message": "Hello, how are you?",
  "stream": false
}
→ 200 OK
{
  "event_id": "event_789",
  "session_id": "session_456",
  "response": "I'm doing well, thank you!",
  "metadata": {
    "model": "gpt-4",
    "tokens": 150
  }
}

# POST /chat/completions (streaming)
Authorization: Bearer {access_token}
{
  "agent_id": "agent_123",
  "message": "Tell me a story",
  "stream": true
}
→ 200 OK (Server-Sent Events)
data: {"type": "start", "event_id": "event_789"}
data: {"type": "content", "delta": "Once"}
data: {"type": "content", "delta": " upon"}
data: {"type": "content", "delta": " a"}
data: {"type": "content", "delta": " time"}
data: {"type": "done"}
```

### Events

```python
# GET /events
Authorization: Bearer {access_token}
?agent_id=agent_123&session_id=session_456&limit=50
→ 200 OK
[
  {
    "event_id": "event_789",
    "event_type": "user_query",
    "content": "Hello",
    "created_at": "2026-02-12T13:00:00Z"
  },
  {
    "event_id": "event_790",
    "event_type": "agent_response",
    "content": "Hi there!",
    "created_at": "2026-02-12T13:00:01Z"
  }
]
```

## Client Implementations

### CLI

```bash
# Login
mo-agent login
# Prompts for username/password
# Stores tokens in ~/.mo-agent/config.json

# Chat
mo-agent chat "Hello"
# Reads access_token from config
# Calls POST /chat/completions
# Auto-refreshes token if expired

# List agents
mo-agent agents list
```

### Python SDK

```python
from mo_agent import AgentEngine

# Initialize
engine = AgentEngine(
    api_url="http://localhost:8000",
    username="alice",
    password="password"
)

# Or with existing token
engine = AgentEngine(
    api_url="http://localhost:8000",
    access_token="eyJhbGc..."
)

# Create agent
agent = engine.create_agent(
    name="My Assistant",
    agent_type="chatbot"
)

# Chat
response = agent.chat("Hello, how are you?")
print(response.content)

# Stream
for chunk in agent.chat("Tell me a story", stream=True):
    print(chunk.delta, end="")
```

### Web UI

```typescript
// React + TypeScript
import { AgentEngineClient } from '@mo-agent/sdk';

const client = new AgentEngineClient({
  apiUrl: 'http://localhost:8000'
});

// Login
await client.auth.login({
  username: 'alice',
  password: 'password'
});

// Chat
const response = await client.chat.completions({
  agentId: 'agent_123',
  message: 'Hello'
});

console.log(response.response);
```

## Service Implementation (FastAPI)

### Project Structure

```
mo-agent-engine/
├── api/
│   ├── __init__.py
│   ├── main.py              # FastAPI app
│   ├── dependencies.py      # Auth dependencies
│   └── routers/
│       ├── auth.py          # /auth/*
│       ├── agents.py        # /agents/*
│       ├── chat.py          # /chat/*
│       ├── events.py        # /events/*
│       └── skills.py        # /skills/*
├── core/
│   ├── auth/
│   │   ├── jwt.py           # JWT generation/validation
│   │   ├── password.py      # Password hashing
│   │   └── oauth2.py        # OAuth2 scheme
│   ├── agent/
│   ├── events/
│   ├── skills/
│   └── context/
├── models/
│   ├── user.py
│   ├── agent.py
│   ├── event.py
│   └── ...
├── schemas/
│   ├── auth.py              # Pydantic schemas
│   ├── agent.py
│   └── ...
├── db/
│   ├── database.py          # Database connection
│   └── migrations/
├── cli/
│   └── mo_agent.py          # CLI (calls API)
├── sdk/
│   └── python/
│       └── mo_agent/
│           └── client.py
└── .env
```

### FastAPI Example

```python
# api/main.py
from fastapi import FastAPI, Depends
from fastapi.security import OAuth2PasswordBearer
from api.routers import auth, agents, chat, events

app = FastAPI(title="Agent Engine API")

app.include_router(auth.router, prefix="/auth", tags=["auth"])
app.include_router(agents.router, prefix="/agents", tags=["agents"])
app.include_router(chat.router, prefix="/chat", tags=["chat"])
app.include_router(events.router, prefix="/events", tags=["events"])

@app.get("/health")
def health_check():
    return {"status": "healthy"}
```

```python
# api/routers/auth.py
from fastapi import APIRouter, Depends, HTTPException
from schemas.auth import RegisterRequest, LoginRequest, TokenResponse
from core.auth.jwt import create_access_token, create_refresh_token
from core.auth.password import verify_password, hash_password
from db.database import get_db

router = APIRouter()

@router.post("/register", response_model=UserResponse)
def register(request: RegisterRequest, db=Depends(get_db)):
    # Check if user exists
    existing = db.fetchone("SELECT * FROM users WHERE username = %s", (request.username,))
    if existing:
        raise HTTPException(400, "Username already exists")
    
    # Create user
    user_id = str(uuid7())
    password_hash = hash_password(request.password)
    db.execute(
        "INSERT INTO users (user_id, username, email, password_hash) VALUES (%s, %s, %s, %s)",
        (user_id, request.username, request.email, password_hash)
    )
    
    return {"user_id": user_id, "username": request.username, "email": request.email}

@router.post("/login", response_model=TokenResponse)
def login(request: LoginRequest, db=Depends(get_db)):
    # Validate credentials
    user = db.fetchone("SELECT * FROM users WHERE username = %s", (request.username,))
    if not user or not verify_password(request.password, user["password_hash"]):
        raise HTTPException(401, "Invalid credentials")
    
    # Generate tokens
    access_token = create_access_token({"sub": user["user_id"], "username": user["username"]})
    refresh_token = create_refresh_token({"sub": user["user_id"]})
    
    # Store refresh token
    store_refresh_token(db, user["user_id"], refresh_token)
    
    return {
        "access_token": access_token,
        "refresh_token": refresh_token,
        "token_type": "bearer",
        "expires_in": 3600
    }
```

## Configuration

### .env

```bash
# Service
API_HOST=0.0.0.0
API_PORT=8000
API_WORKERS=4

# Database (MatrixOne)
DATABASE_HOST=localhost
DATABASE_PORT=6001
DATABASE_USER=app_user
DATABASE_PASSWORD=secret
DATABASE_TENANT=agent_platform  # Configurable, not hardcoded
DATABASE_NAME=agent_engine

# JWT
JWT_SECRET_KEY=your-secret-key-here
JWT_ALGORITHM=HS256
JWT_ACCESS_TOKEN_EXPIRE_MINUTES=60
JWT_REFRESH_TOKEN_EXPIRE_DAYS=7

# CORS
CORS_ORIGINS=http://localhost:3000,http://localhost:8080
```

## Deployment

```bash
# Development
uvicorn api.main:app --reload --host 0.0.0.0 --port 8000

# Production
gunicorn api.main:app -w 4 -k uvicorn.workers.UvicornWorker --bind 0.0.0.0:8000
```

## Summary

### Key Changes from Previous Design

1. ✅ **Service-First**: FastAPI HTTP API, not direct DB access
2. ✅ **Standard OAuth2**: Access token + refresh token
3. ✅ **Configurable Tenant**: Not hardcoded to 'sys', reads from .env
4. ✅ **Universal Agent Engine**: Any agent can use this platform
5. ✅ **Multi-Client**: CLI, Web UI, SDK all consume same API
6. ✅ **User Management**: Full user registration/login flow
7. ✅ **Agent Registry**: Agents register themselves with the engine

### Next Steps

1. Implement FastAPI service
2. Implement authentication (JWT)
3. Implement core API endpoints
4. Update CLI to call API
5. Create Python SDK
6. Create Web UI
