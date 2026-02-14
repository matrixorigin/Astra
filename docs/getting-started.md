# Getting Started

## Prerequisites

- Python 3.11+
- Docker and Docker Compose
- Make

## Setup

```bash
# Clone and install
git clone <repo_url>
cd mo-agent-engine
conda create -n dev-agent python=3.11
conda activate dev-agent
make setup

# Start MatrixOne + Redis
make dev-up

# Verify
curl http://localhost:8000/health
```

## Option 1: CLI

```bash
# Interactive chat
mo-agent chat --user-id alice

# Manage skills
mo-agent skill list
mo-agent skill register skill.json

# Replay a session
mo-agent replay <session_id>
```

## Option 2: API Server

```bash
# Start server
uvicorn api.main:app --reload --port 8000

# Interactive docs
open http://localhost:8000/docs
```

### Quick API Walkthrough

```bash
# 1. Register
curl -X POST http://localhost:8000/auth/register \
  -H "Content-Type: application/json" \
  -d '{"username": "alice", "email": "alice@example.com", "password": "secure_password"}'

# 2. Login → get token
TOKEN=$(curl -s -X POST http://localhost:8000/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username": "alice", "password": "secure_password"}' | jq -r '.access_token')

# 3. Create agent
curl -X POST http://localhost:8000/agents \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"agent_name": "code-reviewer", "system_prompt": "You review code for bugs and style."}'

# 4. Create session
curl -X POST http://localhost:8000/sessions \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"agent_id": "<agent_id>"}'

# 5. Log events
curl -X POST http://localhost:8000/events \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"session_id": "<session_id>", "event_type": "user_query", "content": "Review auth.py"}'
```

## Option 3: Admin CLI

```bash
# Initialize system
mo-admin init

# Manage models
mo-admin model add gpt-4 openai --scope global
mo-admin model list

# Manage API tokens
mo-admin token create --type llm --provider openai --scope global
```

## What's Next

- [API Reference](api-reference.md) — full endpoint documentation
- [Development Guide](development.md) — testing, deployment, code quality
- [Architecture](design/ARCHITECTURE.md) — how the system is designed
