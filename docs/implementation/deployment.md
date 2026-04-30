# Deployment

## Local Development

```bash
make dev-init      # Complete setup: .env + dependencies + config
make dev-start     # Start all services (MatrixOne + Redis + API)
make test          # Run all tests
make dev-stop      # Stop services
```

## Project Structure

```
astra-engine/
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
│   ├── astra_cli (Rust)    # User CLI (chat, skill, session, replay) — `rust/crates/astra-cli`
│   └── astra-admin (Rust)  # Admin CLI — `rust/crates/astra-admin`
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
ASTRA_API_HOST=0.0.0.0 ASTRA_API_PORT=8000 astra-server

# Production
ASTRA_API_HOST=0.0.0.0 ASTRA_API_PORT=8000 astra-server
```

Features: structured JSON logging, JWT auth, rate limiting (60 req/min), health checks, Prometheus metrics.

## Docker

```bash
# Build
docker build -t astra-engine .

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
| `ASTRA_DATABASE` | `astra_runtime` | Platform state database |
| `ASTRA_JWT_SECRET` | (required) | JWT signing secret |
| `ASTRA_TOKEN_ENCRYPTION_KEY` | (required) | Fernet token encryption key |
| `ASTRA_BRIDGE_SECRET` | (required) | Chat turn bridge secret |

LLM provider configuration is managed server-side via the admin CLI, not env vars:

```bash
astra-admin model add gpt-4o-mini openai --api-key sk-... --base-url https://api.openai.com/v1
astra-admin model check gpt-4o-mini                       # activate if reachable
astra-admin config set reasoning_model_name gpt-4o-mini   # (optional) judge/summary model
```

Without an explicit reasoning model, the server picks the cheapest active model by `pricing.completion`.

## Database Initialization

Tables auto-initialize on first API start or `astra-admin init`. Schema is defined in `api/database.py` and `scripts/init-db.sh`.
