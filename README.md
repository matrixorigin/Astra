# mo-dev-agent

Event-centric intelligent agent platform with conversation replay and time-point sandbox capabilities.

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

## Architecture

- **Event-centric design**: All state flows through `conversation_events`
- **Git for Data**: Time-travel queries and zero-copy branching
- **Three-layer model**: Memory → Prompt → Context
- **Reproducibility**: "Ten years later, reproduce today's decision"

## Development

See [docs/design/](docs/design/) for detailed architecture documentation.

## Commands

Run `make help` to see all available commands.

