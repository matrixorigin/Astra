# mo-agent-engine

Intelligent agent platform with auditable decisions, safe iteration, and versioned data lineage.

## Features

- **REST API** ⭐ NEW: Complete HTTP API for authentication, agents, sessions, and events
- **Authentication System** ⭐ NEW: JWT-based auth with access/refresh tokens, user management
- **Agent Management** ⭐ NEW: CRUD operations for agents with ownership verification
- **Session Management** ⭐ NEW: Conversation lifecycle management with metadata
- **Event Tracking** ⭐ NEW: Record and query conversation events with causal chains
- **Auditable Decisions**: Every agent decision binds to a data snapshot — reconstruct what the agent saw at any past moment
- **Auditable Skill Selection**: Every skill selection is versioned and auditable — time-travel to any selection decision
- **Sandbox Pre-Validation**: Validate skill selections in isolated sandbox before execution — choose the best, not guess
- **Self-Improving Selection**: Automatically learn from historical failures using time-travel replay — no manual labeling needed
- **Regression Gate**: Every selector change is automatically tested against golden queries — prevent degradation
- **Event-Centric Architecture**: All interactions stored as atomic events with causal chain tracking
- **Skill System**: Versioned, declarative skills with full replay capability and side-effect isolation
- **Knowledge Regression Detection**: Automatically identify past outputs invalidated by knowledge updates
- **Zero-Cost Experimentation**: Branch production data in milliseconds for testing — zero storage overhead
- **Hallucination Firewall**: Verify LLM claims against the same data snapshot the LLM saw
- **Training Data Pipeline**: Versioned datasets with full lineage from training example to source interaction
- **Cost-Aware Execution**: Predict costs from historical data before spending
- **Multi-Repo Management**: Register and manage multiple repositories with per-repo tokens
- **Production-Ready**: Logging, monitoring, Docker support
- **Type-Safe**: 100% type annotations with Pydantic validation
- **Comprehensive Testing**: 439 tests passing with 99%+ coverage

## Quick Start

### CLI Usage

```bash
# 1. Create and activate virtual environment
conda create -n dev-agent python=3.11
conda activate dev-agent

# 2. Setup environment
make setup

# 3. Start services (MatrixOne + Redis)
make dev-up

# 4. Initialize database
make db-init

# 5. Start using CLI
mo-agent chat

# Or run tests
make test
```

### API Server

```bash
# Start API server
uvicorn api.main:app --reload --port 8000

# Visit interactive docs (Swagger UI)
open http://localhost:8000/docs

# Or ReDoc
open http://localhost:8000/redoc

# Quick test
curl http://localhost:8000/health
```

**API Documentation**:
- Interactive Swagger UI: `http://localhost:8000/docs`
- ReDoc: `http://localhost:8000/redoc`
- [API Reference](docs/API.md) - Detailed examples
- [Quick Start Guide](docs/QUICKSTART.md)

## CLI Commands

### mo-agent (User CLI)

```bash
# Interactive chat
mo-agent chat --user-id alice

# View models
mo-agent model list
mo-agent model show gpt-4

# Manage skills
mo-agent skill list
mo-agent skill register skill.json

# Manage sessions
mo-agent session list
mo-agent session show <session_id>

# Replay conversations
mo-agent replay <session_id>

# Health check
mo-agent health
```

### mo-admin (Admin CLI)

```bash
# Initialize system
mo-admin init

# Manage models
mo-admin model add gpt-4 openai --scope global
mo-admin model add claude-3 anthropic --scope account --scope-id acme
mo-admin model list
mo-admin model remove gpt-4 --scope global

# Manage API tokens
mo-admin token create --type llm --provider openai --scope global
mo-admin token list

# View audit logs
mo-admin audit logs --user alice --since 2026-02-01
```

# Health check
mo-agent health
```

## Usage Example

```python
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from core.sandbox import Sandbox, Branch
from core.repos import RepoRegistry, RepoType, AccessScope, OwnerType
from sdk import Database

# Initialize
db = Database()
session_mgr = SessionManager(db)
logger = EventLogger(db)

# Create session
session = session_mgr.create_session(user_id="alice")

# Log conversation
user_event = logger.create_user_query(
    user_id="alice",
    session_id=session.session_id,
    content="What is event sourcing?",
)

llm_event = logger.create_llm_response(
    user_id="alice",
    session_id=session.session_id,
    content="Event sourcing is...",
    agent_id="dev-agent",
    agent_version="0.1.0",
    parent_event_id=user_event.event_id,
    causal_chain_id=user_event.causal_chain_id,
)

# Multi-repo management
registry = RepoRegistry(db)
repo = registry.create(
    repo_url="https://github.com/matrixorigin/matrixone",
    repo_type=RepoType.CODE,
    owner_id="team_matrixone",
    owner_type=OwnerType.TENANT,
    access_scope=AccessScope.WRITE,
    metadata={"default_branch": "main"}
)

# Sandbox - isolated experiments
sandbox = Sandbox(db=db, account="sys")  # Specify account
sandbox.create("exp1", description="Test", created_by="alice")

# Method 1: Switch to sandbox database
sandbox.use("exp1")
db.execute("SELECT * FROM events")  # Queries exp1.events

# Method 2: Use explicit database name
db.execute("SELECT * FROM exp1.events")

# Manage sandbox
sandbox.add_table("exp1", "conversation_events")  # Add specific table
sandbox.checkpoint("exp1", "before_test")  # Create checkpoint
sandbox.restore("exp1", "before_test")  # Restore using native RESTORE
sandbox.list(pattern="%exp%")  # List with filter
sandbox.delete("exp1")

# Branch - Git-like data workflows
branch = Branch(db=db)
branch.create("events_exp", "conversation_events")
diff = branch.diff("events_exp", "conversation_events", output="count")
branch.merge("events_exp", "conversation_events", on_conflict="accept")
branch.delete("events_exp")
```

See [examples/](examples/) for more detailed examples.

## Core Capabilities

### 1. Event System
- Atomic event logging with full metadata
- Causal chain tracking across multi-turn tool use
- Cross-session queries
- Event integrity validation

### 2. Session Management
- Session lifecycle (create, update, close)
- Event counting and tracking
- Cross-session context with relevance-based retrieval

### 3. Skill System
- Versioned, declarative skills with lifecycle management
- Framework-enforced permissions and requirements
- Side-effect isolation (mock mode for replay)
- Multi-turn tool use with full message chain

### 4. Data Versioning (Git for Data)
- **Decision Audit**: Every decision binds to a data snapshot — query the exact state the agent saw
- **Zero-Cost Branching**: Instant full-data copies for experiments, zero storage overhead
- **Sandbox Isolation**: Database-level isolation for safe experimentation
- **Time-Travel Queries**: Read-only queries at any historical point

### 5. Quality & Safety
- **Regression Gate**: Replay past sessions against changes in isolated environments before deployment
- **Hallucination Firewall**: Verify LLM claims against the same snapshot the LLM saw
- **Knowledge Regression Detection**: Identify past outputs invalidated by knowledge updates
- **Training Data Pipeline**: Versioned datasets with lineage and contamination detection
- **Cost-Aware Execution**: Predict costs from historical data before spending

### 6. Multi-Agent Collaboration (Design)
- Event blackboard coordination — all inter-agent communication through auditable events
- Delegation-as-skill — orchestrator delegates to specialists using existing skill infrastructure
- Fan-out/fan-in, pipeline, adversarial review patterns
- Multi-agent replay with same audit guarantees as single-agent

### 7. Autonomous Planning (Design)
- Plan-Act-Observe-Reflect loop with hierarchical task decomposition
- Plan versioning — every revision is an event, time-travel to any plan state
- Cross-session plan persistence for long-horizon goals
- Plan dry-run in sandbox branches before production execution

### 8. Streaming Output (Design)
- AG-UI protocol aligned structured event stream
- Transport-agnostic: SSE, WebSocket, stdout (CLI)
- Every streamed chunk is a persisted, replayable event
- Multi-agent stream multiplexing with per-agent progress

## Architecture

- **Event-centric**: All state flows through `conversation_events` with causal chains
- **Three-layer context**: Memory (infinite) → Selection → Prompt (finite) → LLM
- **Deterministic boundary control**: Version 4 of 5 decision inputs; constrain LLM non-determinism
- **MatrixOne**: Time-travel, zero-copy branching, HTAP — the data layer that makes audit/regression/lineage possible
- **Type-safe**: Pydantic models throughout, 100% type annotations

## Documentation

- [Development Guide](docs/development.md) - Production setup, testing, deployment
- [Design Documents](docs/design/)
  - [Vision and Mission](docs/design/vision-and-mission.md)
  - [Skills-First Architecture](docs/design/skills-first-architecture.md) ⭐ Phase 2 design
  - [Context Management](docs/design/context-management.md) ⭐ Phase 3 design
  - [Context, Memory, Session and Tables](docs/design/context-memory-session-and-tables.md)
  - [Replay, Sandbox, Evaluation & Evolution](docs/design/replay-sandbox-evaluation-automation.md) ⭐ Engineering validation
  - [Side-Effect Isolation](docs/design/replay-sandbox-evaluation-automation.md#1-side-effect-isolation-critical) ⭐ Critical safety
  - [Multi-Agent Collaboration](docs/design/multi-agent-collaboration.md) ⭐ Event blackboard coordination
  - [Autonomous Planning](docs/design/autonomous-planning.md) ⭐ Plan-Act-Observe-Reflect
  - [Streaming Output](docs/design/streaming-output.md) ⭐ AG-UI protocol alignment
  - [Deployment Architecture](docs/design/deployment-architecture-proposal.md)
  - [GitHub Integration](docs/design/github-integration.md)
  - [LLM Integration](docs/design/llm-integration.md)
  - [Git for Data Features](docs/design/git-for-data-features.md)
  - [Concurrency Model](docs/design/concurrency-model.md)
  - [Hallucination Firewall](docs/design/git-for-data-features.md#5-hallucination-firewall)
  - [Training Data Pipeline](docs/design/git-for-data-features.md#8-training-data-pipeline)
- [Examples](examples/)

## Testing

```bash
# Run all tests
make test

# Run specific test suites
make test-unit          # Unit tests
make test-integration   # Integration tests
```

Current test coverage: **79 tests, 100% passing**

---

## Production Deployment

### Quick Deploy

```bash
# 1. Configure
cp .env.production.example .env.production
# Edit with your secrets

# 2. Deploy
docker-compose -f docker-compose.prod.yml up -d

# 3. Verify
curl http://localhost:8000/health
```

### Features

- ✅ Structured logging (JSON)
- ✅ API authentication (API Key + JWT)
- ✅ Rate limiting (60 req/min)
- ✅ Health checks (3 endpoints)
- ✅ Prometheus metrics
- ✅ Docker support
- ✅ Environment-based configuration

See [Development Guide](docs/development.md) for details.

---

## Project Status

✅ MVP Complete — Core functionality implemented and tested:
- Event system with causal chain tracking
- Session management with cross-session context
- Skill system with versioning and side-effect isolation
- Git for Data integration (time machine + sandbox)
- 79 tests, 100% passing

## Development

See [docs/design/](docs/design/) for detailed architecture documentation.

## Commands

Run `make help` to see all available commands.

### Code Quality
```bash
make check          # Run all static checks (lint + type-check)
make lint           # Run ruff linter
make lint-fix       # Auto-fix linting issues
make type-check     # Run mypy type checker
make format         # Format code
```

See [STATIC_CHECK_SETUP.md](STATIC_CHECK_SETUP.md) for detailed static checking documentation.

