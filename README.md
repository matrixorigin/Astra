# mo-agent-engine

Intelligent agent platform with auditable decisions, safe iteration, and versioned data lineage.

## Features

### Core API (Complete)
- **REST API** ✅ Complete HTTP API with 9 resource endpoints
- **Authentication System** ✅ JWT-based auth with access/refresh tokens, user management
- **Agent Management** ✅ CRUD operations for agents with ownership verification
- **Session Management** ✅ Conversation lifecycle management with metadata
- **Event Tracking** ✅ Record and query conversation events with causal chains
- **Sandbox Management** ✅ Isolated environments for safe experimentation
- **Session Replay** ✅ Replay conversations for testing and validation
- **Skill Registry** ✅ Version-controlled skill management
- **Context Snapshots** ✅ Capture complete context for every decision
- **Decision Audit** ✅ Full audit trail linking decisions to context snapshots

### Auditable Decisions (Implemented)
- Every agent decision binds to a data snapshot — reconstruct what the agent saw at any past moment
- Context snapshots capture: system prompt, skill definitions, selected events, code context, documentation
- Decision audit trail: decision type, output, model parameters, linked to snapshot
- Time-travel capability: query exact state at any historical point

### Self-Improving Selector (Phase 1 Complete)
- **Multi-Dimensional Learning**: Learn from 4 signal types (wrong skill, slow execution, high cost, low satisfaction)
- **Multi-Factor Scoring**: Weighted scoring across accuracy, speed, cost, satisfaction
- **Regression Gate**: Validate learning before deployment
- **REST API**: 5 endpoints for learning management
- **Runtime Configuration**: Configurable weights and decay per signal type

### Additional Features
- **Auditable Skill Selection**: Every skill selection is versioned and auditable
- **Sandbox Pre-Validation**: Validate skill selections in isolated sandbox before execution
- **Event-Centric Architecture**: All interactions stored as atomic events with causal chain tracking
- **Skill System**: Versioned, declarative skills with full replay capability
- **Multi-Repo Management**: Register and manage multiple repositories with per-repo tokens
- **Memory Governance** ✅ Automated lifecycle management: confidence decay, quarantine, compression with distributed scheduling
- **Hybrid Retrieval** ✅ Vector + fulltext search with semantic/keyword/temporal/causal scoring
- **Production-Ready**: Logging, monitoring, Docker support
- **Type-Safe**: 100% type annotations with Pydantic validation
- **Comprehensive Testing**: 820 tests passing with real database integration
- **Side-Effect Isolation**: ToolMockingLayer prevents real-world side effects during replay

## Quick Start

### Development (Recommended)

```bash
# 1. Setup environment
conda create -n dev-agent python=3.11
conda activate dev-agent
make setup

# 2. Initialize and start (< 10 seconds)
make dev-init          # Auto-generate keys, fix config
make dev-start         # Start all services

# 3. Check status
make dev-status

# 4. Visit interactive docs
open http://localhost:8000/docs
```

### Production (Docker)

```bash
# 1. Configure environment
cp .env.example .env
# Edit .env: set TOKEN_ENCRYPTION_KEY, JWT_SECRET_KEY, LLM tokens

# 2. Start all services
make dev-start-docker

# 3. Visit API
open http://localhost:8000/docs
```

### Daily Development

```bash
make dev-start         # Start everything
make dev-api-restart   # Restart API after code changes
make dev-test-keep     # Run tests
make dev-stop          # Stop everything
```

## Documentation

📖 **[Documentation Hub](docs/README.md)** - Start here for all documentation

### Quick Links

- 🚀 **[5-Minute Quick Start](docs/quickstart/README.md)** - Get running fast
- 📘 **[Development Workflow](docs/guides/development-workflow.md)** - Daily development guide
- 📚 **[API Reference](docs/reference/api-reference.md)** - Complete API documentation
- 🔧 **[Makefile Commands](docs/reference/makefile-commands.md)** - All available commands
- 🆘 **[Troubleshooting](docs/guides/troubleshooting.md)** - Common issues and solutions

### By Topic

**Getting Started:**
- [Development Setup](docs/quickstart/development.md)
- [Docker Deployment](docs/quickstart/docker.md)
- [Production Deployment](docs/quickstart/production.md)

**Guides:**
- [Testing Guide](docs/guides/testing.md)
- [Deployment Guide](docs/guides/deployment.md)

**Reference:**
- [CLI Commands](docs/reference/cli-commands.md)
- [Configuration](docs/reference/configuration.md)

**Design & Architecture:**
- [Architecture](docs/design/ARCHITECTURE.md)
- [Memory Architecture](docs/design/memory-architecture.md)
- [Trust and Safety](docs/design/trust-and-safety.md)
- [All Design Docs](docs/design/)

## API Endpoints

### Authentication
- `POST /auth/register` - Register new user
- `POST /auth/login` - Login and get JWT token
- `POST /auth/refresh` - Refresh access token
- `GET /auth/me` - Get current user info

### Chat
- `POST /chat` - Send message, get response (auto-creates session if omitted, returns run_id)
- `POST /chat/stream` - Stream chat response as SSE (returns run_id in first event)
- `GET /chat/runs/{run_id}` - Get run status and progress
- `GET /chat/runs/{run_id}/stream` - Stream run events (supports reconnection)
- `DELETE /chat/runs/{run_id}` - Cancel a running task

### Background Jobs
- `POST /jobs` - Submit background job (training, data collection, etc.)
- `GET /jobs/{job_id}` - Get job status and result
- `DELETE /jobs/{job_id}` - Cancel running job
- `POST /jobs/webhook` - Job completion webhook (resumes waiting agent runs)

### Workflows
- `GET /workflows` - List registered workflow definitions
- `GET /workflows/runs/{run_id}` - Get workflow run status and step results
- `POST /workflows/runs/{run_id}/resolve` - Resolve a wait step (e.g. human approval)

### Triggers
- `POST /triggers` - Create webhook or cron trigger
- `GET /triggers` - List your triggers
- `DELETE /triggers/{trigger_id}` - Delete a trigger
- `POST /triggers/{trigger_id}/fire` - Fire a webhook trigger (secret-based auth)

### Agents
- `POST /agents` - Create agent
- `GET /agents` - List agents
- `GET /agents/{agent_id}` - Get agent
- `PUT /agents/{agent_id}` - Update agent
- `DELETE /agents/{agent_id}` - Delete agent

### Sessions
- `POST /sessions` - Create session
- `GET /sessions` - List sessions
- `GET /sessions/{session_id}` - Get session
- `PUT /sessions/{session_id}` - Update session
- `POST /sessions/{session_id}/close` - Close session
- `DELETE /sessions/{session_id}` - Delete session

### Events
- `POST /events` - Create event
- `GET /events` - List events
- `GET /events/{event_id}` - Get event
- `GET /events/session/{session_id}` - Get session events
- `GET /events/causal-chain/{chain_id}` - Get causal chain
- `DELETE /events/{event_id}` - Delete event

### Sandbox
- `POST /sandbox` - Create sandbox
- `GET /sandbox` - List sandboxes
- `GET /sandbox/{name}` - Get sandbox info
- `DELETE /sandbox/{name}` - Delete sandbox

### Replay
- `POST /sessions/{session_id}/replay` - Replay session
- `GET /sessions/{session_id}/replay/compare` - Compare replay results

### Skills
- `POST /skills` - Register skill
- `GET /skills` - List skills
- `GET /skills/{skill_id}` - Get skill
- `GET /skills/{skill_id}/versions` - List skill versions

### Context Snapshots
- `POST /context` - Create context snapshot
- `GET /context` - List snapshots
- `GET /context/{snapshot_id}` - Get snapshot

### Decision Audit
- `POST /decisions` - Record decision
- `GET /decisions` - List decisions
- `GET /decisions/{decision_id}` - Get decision
- `GET /decisions/{decision_id}/audit` - Audit decision (with full context)

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
from sqlalchemy.orm import Session
from api.database import get_db_session

# Initialize
db = next(get_db_session())
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

### 6. Multi-Agent Collaboration ✅
- Event blackboard coordination — all inter-agent communication through auditable events
- Delegation-as-skill — orchestrator delegates to specialists using existing skill infrastructure
- Fan-out/fan-in, pipeline, adversarial review patterns
- Multi-agent replay with same audit guarantees as single-agent

### 7. Autonomous Planning ✅
- Plan-Act-Observe-Reflect loop with hierarchical task decomposition
- Plan versioning — every revision is an event, time-travel to any plan state
- Cross-session plan persistence for long-horizon goals
- Plan dry-run in sandbox branches before production execution

### 8. Streaming Output ✅
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

## Testing

```bash
make dev-test-keep      # Run all tests (keeps services running)
make dev-test           # Run all tests (stops services after)
make dev-test-unit      # Unit tests only
make dev-test-integration  # Integration tests only
```

See [Testing Guide](docs/guides/testing.md) for detailed testing documentation.

---

## Deployment

### Docker Compose (All-in-One)

```bash
# Start everything
cd deployment/all-in-one && docker-compose up -d

# Verify
curl http://localhost:8000/health
```

### Kubernetes

```bash
helm install mo-agent deployment/kubernetes/chart
```

See [deployment/](deployment/) for all options (GPU, Ray, external DB, etc.).

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
