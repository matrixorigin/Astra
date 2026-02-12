# Agent Engine API - Quick Start Guide

Get started with the Agent Engine API in 5 minutes.

## Prerequisites

- Python 3.10+
- MatrixOne database running
- Redis (optional, for future features)

## Installation

```bash
# 1. Clone repository
git clone <repo_url>
cd mo-dev-agent

# 2. Create virtual environment
conda create -n dev-agent python=3.11
conda activate dev-agent

# 3. Install dependencies
make install

# 4. Setup environment
cp .env.example .env
# Edit .env with your configuration

# 5. Start database
make dev-up

# 6. Initialize database
make db-init
```

## Start API Server

```bash
# Development mode (with auto-reload)
uvicorn api.main:app --reload --host 0.0.0.0 --port 8000

# Production mode
uvicorn api.main:app --host 0.0.0.0 --port 8000 --workers 4
```

Server will be available at: `http://localhost:8000`

## Quick Test

### 1. Check Health

```bash
curl http://localhost:8000/health
```

### 2. Register User

```bash
curl -X POST http://localhost:8000/auth/register \
  -H "Content-Type: application/json" \
  -d '{
    "username": "alice",
    "email": "alice@example.com",
    "password": "secure_password",
    "display_name": "Alice"
  }'
```

### 3. Login

```bash
curl -X POST http://localhost:8000/auth/login \
  -H "Content-Type: application/json" \
  -d '{
    "username": "alice",
    "password": "secure_password"
  }'
```

Save the `access_token` from the response.

### 4. Create Agent

```bash
curl -X POST http://localhost:8000/agents \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <your_access_token>" \
  -d '{
    "agent_name": "My Assistant",
    "agent_type": "chatbot",
    "config": {"model": "gpt-4"}
  }'
```

### 5. Create Session

```bash
curl -X POST http://localhost:8000/sessions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <your_access_token>" \
  -d '{
    "metadata": {"context": "demo"}
  }'
```

Save the `session_id` from the response.

### 6. Create Event

```bash
curl -X POST http://localhost:8000/events \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer <your_access_token>" \
  -d '{
    "session_id": "<your_session_id>",
    "event_type": "user_query",
    "content": "Hello, world!"
  }'
```

## Interactive Documentation

Visit `http://localhost:8000/docs` for:
- Interactive API explorer
- Try out endpoints
- View request/response schemas
- Download OpenAPI spec

## Python Client Example

```python
import requests

BASE_URL = "http://localhost:8000"

# Login
response = requests.post(
    f"{BASE_URL}/auth/login",
    json={"username": "alice", "password": "secure_password"}
)
access_token = response.json()["access_token"]

# Create session
headers = {"Authorization": f"Bearer {access_token}"}
response = requests.post(
    f"{BASE_URL}/sessions",
    headers=headers,
    json={"metadata": {"context": "demo"}}
)
session_id = response.json()["session_id"]

# Create event
response = requests.post(
    f"{BASE_URL}/events",
    headers=headers,
    json={
        "session_id": session_id,
        "event_type": "user_query",
        "content": "Hello!"
    }
)
print(response.json())
```

See `examples/api_usage_example.py` for complete workflow.

## Next Steps

- Read [API Documentation](API.md) for detailed endpoint reference
- Explore [Architecture Design](design/) for system overview
- Check [Development Guide](development.md) for contribution guidelines
- Run tests: `make test`

## Troubleshooting

### Database Connection Error

```bash
# Check if MatrixOne is running
make dev-ps

# Restart services
make dev-restart

# Check logs
make dev-logs-db
```

### Authentication Error

- Verify JWT_SECRET_KEY in `.env` (must be 32+ characters)
- Check token expiration (default: 1 hour for access token)
- Use `/auth/refresh` to get new access token

### Port Already in Use

```bash
# Change port in uvicorn command
uvicorn api.main:app --port 8001
```

## Support

- Documentation: `docs/`
- Examples: `examples/`
- Tests: `tests/integration/api/`
- Issues: GitHub Issues
