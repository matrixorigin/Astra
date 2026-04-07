# Development Workflow Guide

## Quick Start (Most Common)

```bash
# First time setup
make dev-init           # Complete setup: .env + dependencies + config

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
| `make dev-deps-wait` | Wait for dependencies (max 20s) |
| `make dev-db-connect` | Connect to MatrixOne CLI |

### API Server (Source Code Mode)

| Command | Description |
|---------|-------------|
| `make dev-api-start` | Start API server |
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
| `make test` | Run all Rust workspace tests |
| `make test-contract` | Run `http_contract` / `admin_contract` (astra-runtime) + settings JSON contract (`astra-core` `settings_contract_tests`) |

## Typical Workflows

### Daily Development

```bash
# Morning
make dev-start          # Start everything
make dev-status         # Verify ready

# Development loop
# ... edit code ...
make dev-api-restart    # Restart API after changes
make test               # Run tests

# Evening
make dev-stop           # Stop everything
```

### Testing Changes

```bash
# Quick: run all tests
make test

# Targeted: run integration contracts only
make test-contract

# Specific: run individual contract suites
make test-contract
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

If you're behind a corporate proxy:

```bash
export NO_PROXY=localhost,127.0.0.1
```

## Troubleshooting

### Dependencies not ready after dev-start

Dependencies (especially MatrixOne) may take 30-60s to fully start:

```bash
make dev-status
make dev-deps-status
```

### API server won't start

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
make dev-deps-wait
```
