# mo-dev-agent

Event-centric intelligent agent platform with conversation replay and time-point sandbox capabilities.

## Features

- **Event-Centric Architecture**: All conversations stored as atomic events with full causality tracking
- **Session Management**: Complete conversation lifecycle management
- **Git for Data**: Time-travel queries and isolated sandbox experiments using MatrixOne snapshots
- **Type-Safe**: 100% type annotations with Pydantic validation
- **Production-Ready**: Comprehensive test coverage (25+ tests)

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

### 3. Git for Data
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

- [Development Guide](docs/development.md)
- [Design Documents](docs/design/)
- [Examples](examples/)

## Testing

```bash
# Run all tests
make test

# Run specific test suites
make test-unit          # Unit tests
make test-integration   # Integration tests
```

Current test coverage: **25 tests, 100% passing**

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

