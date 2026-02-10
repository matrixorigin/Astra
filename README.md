# mo-dev-agent

Event-centric intelligent agent platform with conversation replay and time-point sandbox capabilities.

## Features

- **Event-Centric Architecture**: All conversations stored as atomic events with full causality tracking
- **Session Management**: Complete conversation lifecycle management
- **Multi-Repo Management**: Register and manage multiple repositories with per-repo tokens
- **Skill System**: Versioned, declarative skills with full replay capability
- **Git for Data**: Time-travel queries and isolated sandbox experiments
- **Production-Ready**: Logging, auth, rate limiting, monitoring, Docker support
- **Type-Safe**: 100% type annotations with Pydantic validation
- **Comprehensive Testing**: 79 tests with VCR-recorded real API responses

## Quick Start

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

# 5. Run tests
make test
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
- Causal chain tracking
- Cross-session queries
- Event integrity validation

### 2. Session Management
- Session lifecycle (create, update, close)
- Event counting and tracking
- Custom metadata support

### 3. Multi-Repo Management
- Register multiple repositories (code, CI, tester, docs)
- Per-repo token management
- Owner-based access control (user/tenant)
- Flexible metadata storage

### 4. Git for Data
- **Time Machine**: Query data at any point in time (read-only)
- **Sandbox**: Database-level isolation for experiments
- **Branch**: Table-level Git-like workflows (create, diff, merge)
- **Zero-copy CLONE**: Instant duplication with no storage overhead

## Architecture

- **Event-Centric**: Single source of truth for all conversation data
- **MatrixOne**: Hyper-converged database with Git for Data capabilities
- **Type-Safe**: Pydantic models for all data structures
- **Testable**: Comprehensive unit and integration tests

## Documentation

- [Development Guide](docs/development.md) - Production setup, testing, deployment
- [Design Documents](docs/design/)
  - [Vision and Mission](docs/design/vision-and-mission.md)
  - [Skills-First Architecture](docs/design/skills-first-architecture.md) ⭐ Phase 2 design
  - [Context Management](docs/design/context-management.md) ⭐ Phase 3 design
  - [Context, Memory, Session and Tables](docs/design/context-memory-session-and-tables.md)
  - [Deployment Architecture](docs/design/deployment-architecture-proposal.md)
  - [GitHub Integration](docs/design/github-integration.md) ⭐ Industry-leading
  - [LLM Integration](docs/design/llm-integration.md) ⭐ Production-ready
  - [Git for Data Features](docs/design/git-for-data-features.md)
  - [Concurrency Model](docs/design/concurrency-model.md)
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

✅ MVP Complete - Core functionality implemented and tested:
- Event system with causal chain tracking
- Session management
- Git for Data integration (time machine + sandbox)
- Comprehensive test coverage

## Architecture

- **Event-centric design**: All state flows through `conversation_events`
- **Git for Data**: Time-travel queries and zero-copy branching
- **Three-layer model**: Memory → Prompt → Context
- **Reproducibility**: "Ten years later, reproduce today's decision"

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

