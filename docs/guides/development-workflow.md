# Development Workflow Guide

## Quick Start (Most Common)

```bash
# First time setup
make dev-init           # Complete setup: .env + dependencies + config

# Daily development
make dev-start-server-only # Start deps + API + Web without a local edge provider
make dev-status         # Check if everything is ready

# Web sessions that need local files/shell/git
make dev-start-server-edge # Also starts astra-edge for this repo

# After code changes
make dev-api-restart    # Restart API server only

# Stop everything
make dev-stop
```

## Command Reference

### Quick Start Commands

| Command                      | Description                                                      | Time    |
| ---------------------------- | ---------------------------------------------------------------- | ------- |
| `make dev-start`             | Alias for `make dev-start-server-only`                           | ~5s     |
| `make dev-start-server-only` | Start deps + API + Web; server-service tools only, no edge tools | ~5s     |
| `make dev-start-server-edge` | Start server-only, then connect local `astra-edge`               | ~10s    |
| `make dev-start-docker`      | Start deps + API in Docker mode                                  | ~10s    |
| `make dev-stop`              | Stop Web, API, deps, and local `astra-edge` if running           | ~2s     |
| `make dev-status`            | Show dependency, API, Web, and edge-provider status              | instant |
| `make dev-init`              | Initialize environment                                           | ~10s    |

### Dependency Services (MatrixOne + Memoria)

| Command                | Description                      |
| ---------------------- | -------------------------------- |
| `make dev-deps-up`     | Start dependencies               |
| `make dev-deps-down`   | Stop dependencies                |
| `make dev-deps-clean`  | Delete all data (⚠️ destructive) |
| `make dev-deps-status` | Show dependency status           |
| `make dev-deps-logs`   | Tail all dependency logs         |
| `make dev-deps-wait`   | Wait for dependencies (max 20s)  |
| `make dev-db-connect`  | Connect to MatrixOne CLI         |

### API Server (Source Code Mode)

| Command                | Description                     |
| ---------------------- | ------------------------------- |
| `make dev-api-start`   | Start API server                |
| `make dev-api-stop`    | Stop API server                 |
| `make dev-api-restart` | Restart API server              |
| `make dev-api-logs`    | Tail API server logs            |
| `make dev-api-status`  | Show API server status + health |

### API Server (Docker Mode)

| Command                                | Description            |
| -------------------------------------- | ---------------------- |
| `make dev-api-docker-build`            | Build API server image |
| `make dev-api-docker-up`               | Start API container    |
| `make dev-api-docker-down`             | Stop API container     |
| `make dev-api-docker-logs`             | Tail container logs    |
| `make dev-api-docker-scale REPLICAS=N` | Scale to N replicas    |

### Runtime Profiles

| Profile          | Command                      | Use when                                                                 |
| ---------------- | ---------------------------- | ------------------------------------------------------------------------ |
| Server-only      | `make dev-start-server-only` | Testing Web agent backbone, server-service tools, memory, planning, MCP. |
| Server + edge    | `make dev-start-server-edge` | Testing Web access to local files, shell, git, private networks.         |
| Docker server    | `make dev-start-docker`      | Testing API container packaging without a local edge provider.           |
| Docker + edge    | `make dev-start-docker && make dev-edge-start` | Testing a containerized API with a host `astra-edge`.       |

`astra-edge` reads the selected Astra CLI profile token by default. Run
`astra login` first, or set `ASTRA_TOKEN` explicitly. Override the local
workspace with `ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo make dev-edge-start`.

### Testing

| Command              | Description                                                                                                              |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `make test`          | Run all Rust workspace tests                                                                                             |
| `make test-server-only` | Focused server-only Web/runtime tests                                                                                 |
| `make test-server-edge` | Focused edge provider protocol and routing tests                                                                       |
| `make test-contract` | Run `http_contract` / `admin_contract` (astra-runtime) + settings JSON contract (`astra-core` `settings_contract_tests`) |

## Typical Workflows

### Daily Development

```bash
# Morning
make dev-start-server-only # Start server-only profile
make dev-status         # Verify ready

# Need local workspace tools from Web
make dev-start-server-edge

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

# Optional: connect host local workspace provider
make dev-edge-start

# Scale up
make dev-api-docker-scale REPLICAS=4

# Check logs
make dev-api-docker-logs

# Stop
make dev-stop
```

### Clean Slate

```bash
# Destructive local reset: stop services and remove local dependency data
make dev-clean          # Prompts for confirmation

# Reinitialize
make dev-init
make dev-start
```

## Environment Variables

The `dev-init` command automatically:

- Generates `ASTRA_TOKEN_ENCRYPTION_KEY` if missing
- Generates `ASTRA_JWT_SECRET` if missing
- Generates `ASTRA_RUNTIME_ROOT_SECRET` if missing or still using the template placeholder

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

- Port 17001 already in use: `lsof -i :17001` and kill the process
- Dependencies not ready: Wait and retry
- **Missing JWT secret / runtime root secret**: If you see `MissingRequiredKey` for `ASTRA_JWT_SECRET` or `ASTRA_RUNTIME_ROOT_SECRET`:
  - Run `make dev-init` (auto-generates secrets in `.env`)
  - Or set `ASTRA_ALLOW_INSECURE_DEFAULTS=1` for quick dev (NOT for production)

### Tests failing

Ensure dependencies are running:

```bash
make dev-deps-status
make dev-deps-wait
```
