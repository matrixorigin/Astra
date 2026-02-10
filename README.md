# mo-dev-agent

Event-centric intelligent agent platform with conversation replay and time-point sandbox capabilities.

## Quick Start

```bash
# 1. Setup environment
make setup

# 2. Start services (MatrixOne + Redis)
make dev-up

# 3. Initialize database
make db-init

# 4. Run tests
make test
```

## Architecture

- **Event-centric design**: All state flows through `conversation_events`
- **Git for Data**: Time-travel queries and zero-copy branching
- **Three-layer model**: Memory → Prompt → Context
- **Reproducibility**: "Ten years later, reproduce today's decision"

## Development

See [docs/design/](docs/design/) for detailed architecture documentation.

## Commands

Run `make help` to see all available commands.

