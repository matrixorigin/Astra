# Development Workflow Guide

## Quick Start (Most Common)

```bash
# First time setup
make setup              # Copy .env, install dependencies
make dev-init           # Initialize environment (generate keys, fix config)

# Daily development
make dev-start          # Start all services (< 10 seconds)
make dev-status         # Check if everything is ready

# After code changes
make dev-api-restart    # Restart API server only

# Stop everything
make dev-stop
```

## Command Reference

### Quick Start Commands

| Command | Description | Time |
|---------|-------------|------|
| `make dev-start` | Start deps + API (source mode) | ~5s |
| `make dev-start-docker` | Start deps + API (Docker mode) | ~10s |
| `make dev-stop` | Stop all services | ~2s |
| `make dev-status` | Show all service status | instant |
| `make dev-init` | Initialize environment | ~10s |

### Dependency Services (MatrixOne + Redis)

| Command | Description |
|---------|-------------|
| `make dev-deps-up` | Start dependencies |
| `make dev-deps-down` | Stop dependencies |
| `make dev-deps-clean` | Delete all data (⚠️ destructive) |
| `make dev-deps-status` | Show dependency status |
| `make dev-deps-logs` | Tail all dependency logs |
| `make dev-deps-logs-db` | Tail MatrixOne logs only |
| `make dev-deps-logs-redis` | Tail Redis logs only |
| `make dev-deps-wait` | Wait for dependencies (max 10s) |
| `make dev-db-connect` | Connect to MatrixOne CLI |

### API Server (Source Code Mode)

| Command | Description |
|---------|-------------|
| `make dev-api-start` | Start API server (hot reload) |
| `make dev-api-stop` | Stop API server |
| `make dev-api-restart` | Restart API server |
| `make dev-api-logs` | Tail API server logs |
| `make dev-api-status` | Show API server status + health |

### API Server (Docker Mode)

| Command | Description |
|---------|-------------|
| `make dev-api-docker-build` | Build API server image |
| `make dev-api-docker-up` | Start API container |
| `make dev-api-docker-down` | Stop API container |
| `make dev-api-docker-logs` | Tail container logs |
| `make dev-api-docker-scale REPLICAS=N` | Scale to N replicas |

### Testing

| Command | Description |
|---------|-------------|
| `make dev-test` | Run all tests (auto cleanup) |
| `make dev-test-keep` | Run all tests (keep deps) |
| `make dev-test-unit` | Run unit tests only |
| `make dev-test-integration` | Run integration tests |

## Typical Workflows

### Daily Development

```bash
# Morning
make dev-start          # Start everything
make dev-status         # Verify ready

# Development loop
# ... edit code ...
make dev-api-restart    # Restart API after changes
make dev-test-keep      # Run tests

# Evening
make dev-stop           # Stop everything
```

### Testing Changes

```bash
# Quick test (auto cleanup)
make dev-test

# Repeated testing (keep deps running)
make dev-deps-up
make dev-test-unit      # Fast unit tests
make dev-test-integration  # Integration tests
make dev-deps-down
```

### Docker Mode (Multi-replica Testing)

```bash
# Start with Docker
make dev-start-docker

# Scale up
make dev-api-docker-scale REPLICAS=4

# Check logs
make dev-api-docker-logs

# Stop
make dev-stop
```

### Clean Slate

```bash
# Nuclear option: delete everything
make dev-clean          # Will prompt for confirmation

# Reinitialize
make dev-init
make dev-start
```

## Environment Variables

The `dev-init` command automatically:
- Generates `TOKEN_ENCRYPTION_KEY` if missing
- Fixes `OPENAI_AKI_KEY` → `OPENAI_API_KEY` typo
- Validates LLM provider/model configuration

## Proxy Configuration

If you're behind a corporate proxy, prefix commands with `NO_PROXY`:

```bash
NO_PROXY=localhost mo-agent register
NO_PROXY=localhost mo-agent login
NO_PROXY=localhost mo-agent chat
```

Or add to your shell profile:
```bash
export NO_PROXY=localhost,127.0.0.1
```

## Troubleshooting

### Dependencies not ready after dev-start

Dependencies (especially MatrixOne) may take 30-60s to fully start. Check status:

```bash
make dev-status
# Wait a bit, then check again
make dev-deps-status
```

### API server won't start

Check logs:
```bash
make dev-api-logs
```

Common issues:
- Port 8000 already in use: `lsof -i :8000` and kill the process
- Dependencies not ready: Wait and retry

### Tests failing

Ensure dependencies are running:
```bash
make dev-deps-status
make dev-deps-wait      # Wait up to 10s
```

## Migration from Old Commands

| Old Command | New Command | Notes |
|-------------|-------------|-------|
| `make dev-up` | `make dev-deps-up` | More explicit |
| `make dev-full` | `make dev-start-docker` | Clearer intent |
| `make dev-ps` | `make dev-status` | More comprehensive |

Old commands still work but show deprecation warnings.
