# Deployment

## Local Development

```bash
make setup      # Install dependencies
make dev-up     # Start MatrixOne + Redis (Docker Compose)
make test       # Run all tests (527 tests)
make dev-down   # Stop services
```

## Project Structure

```
mo-agent-engine/
├── api/                    # FastAPI REST API
│   ├── main.py             # Application entry, middleware, CORS
│   ├── routers/            # Endpoint handlers (agents, sessions, events, ...)
│   ├── services/           # Business logic
│   ├── repositories/       # Database access
│   ├── models.py           # Pydantic request/response models
│   └── database.py         # SQLAlchemy session management
│
├── core/                   # Platform engine
│   ├── agent/              # ChatLoop, Planner (PAOR), AgentManager
│   ├── events/             # EventLogger, SessionManager, causal chains
│   ├── context/            # ContextManager, prompts, embeddings, scorer
│   ├── skills/             # SkillRegistry, selector, auditable selector
│   ├── llm/                # LLMClient, providers, router, rate limiter
│   ├── sandbox/            # Sandbox (clone), Branch (diff/merge)
│   ├── replay/             # TimeMachine, SemanticDiff (session replay via api/services/replay_service)
│   ├── auth/               # UserManager, PermissionChecker, AuditLogger
│   ├── repos/              # RepoRegistry, TokenResolver
│   ├── scope/              # ScopeResolver (scope-based config)
│   ├── verification/       # HallucinationFirewall
│   ├── query/              # Natural language query
│   ├── git_for_data.py     # Snapshot, time-travel, clone
│   └── validation.py       # Input validation utilities
│
├── cli/                    # Command-line interfaces
│   ├── mo_agent.py         # User CLI (chat, skill, session, replay)
│   └── mo_admin.py         # Admin CLI (init, model, token, audit)
│
├── config/                 # Configuration
│   └── settings.py         # Environment-based settings
│
├── tests/
│   ├── unit/               # ~300 unit tests
│   └── integration/        # ~200 integration tests (real DB)
│
├── scripts/                # Utility scripts
├── sdk/                    # Client SDK (future)
└── examples/               # Usage examples
```

## API Server

```bash
# Development
uvicorn api.main:app --reload --port 8000

# Production
uvicorn api.main:app --host 0.0.0.0 --port 8000 --workers 4
```

Features: structured JSON logging, JWT auth, rate limiting (60 req/min), health checks, Prometheus metrics.

## Docker

```bash
# Build
docker build -t mo-agent-engine .

# All-in-one (MatrixOne + Redis + API)
cd deployment/all-in-one && docker-compose up -d

# With GPU + model server
cd deployment/all-in-one && docker-compose --profile full up -d
```

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `MATRIXONE_HOST` | `localhost` | MatrixOne host |
| `MATRIXONE_PORT` | `6001` | MatrixOne port |
| `MATRIXONE_USER` | `root` | MatrixOne user |
| `MATRIXONE_PASSWORD` | `111` | MatrixOne password |
| `MATRIXONE_DATABASE` | `dev_agent` | Platform state database |
| `REDIS_URL` | `redis://localhost:6379` | Redis URL |
| `JWT_SECRET` | (required) | JWT signing secret |
| `OPENAI_API_KEY` | (optional) | OpenAI provider |
| `ANTHROPIC_API_KEY` | (optional) | Anthropic provider |
| `GROQ_API_KEY` | (optional) | Groq provider |

## Database Initialization

Tables auto-initialize on first API start or `mo-admin init`. Schema is defined in `api/database.py` and `scripts/init-db.sh`.
