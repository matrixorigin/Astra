# Development Workflow Guide

## Quick Start (Most Common)

```bash
# First checkout: install pinned dependencies and create local config
cp .models.yaml.example .models.yaml
make dev-init

# Configure embeddings in .env and at least one model in .models.yaml.

# First start: establish identity before connecting a User Runner
make build-cli-debug
make dev-start          # Explicit Server-only default: deps + API + Web
./target/debug/astra admin register
./target/debug/astra admin model load .models.yaml --update-existing

# After login, connect this checkout as a User Runner when needed
ASTRA_EDGE_WORKSPACE_DIR="$PWD" make dev-edge-start

# Normal edit loop
make dev-api-restart-debug
make test-contract

make dev-stop
```

`make dev-start` is intentionally an alias for the Server-only profile. It
always disconnects the Edge process previously launched by this checkout, so
startup state is reproducible without touching independently managed Runners.
After the first login, `make dev-start-server-edge` starts the same backbone
and reconnects the User Runner in one command.

## Command Reference

### Quick Start Commands

| Command                      | Description                                                      |
| ---------------------------- | ---------------------------------------------------------------- |
| `make dev-start`             | Alias for the deterministic Server-only profile                  |
| `make dev-start-server-only` | Start deps + API + Web; disconnect any repo-launched Edge        |
| `make dev-start-server-edge` | Start Server-only, then connect this checkout as a User Runner   |
| `make dev-start-docker`      | Start dependencies plus the configured API container image      |
| `make dev-stop`              | Stop Web, API, dependencies, and the repo-launched Edge process  |
| `make dev-status`            | Show dependency, API, Web, and Edge status                       |
| `make dev-init`              | Create config and install pinned Rust/Node dependencies          |

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

Run `make dev-api-docker-build` first when you need the container to include
unpublished server changes from the current checkout.

`astra-edge` reads the selected Astra CLI profile token by default. Run
`astra login` first, or set `ASTRA_TOKEN` explicitly. Override the local
workspace with `ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo make dev-edge-start`.

### Testing

| Command              | Description                                                                                                              |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `make test`          | Run the complete offline and online repository suite; requires live development dependencies                            |
| `make test-offline`  | Run the offline Rust, SDK, Web, hook, and runtime-profile gates                                                          |
| `make test-server-only` | Focused server-only Web/runtime tests                                                                                 |
| `make test-server-edge` | Focused edge provider protocol and routing tests                                                                       |
| `make test-contract` | Run `http_contract` / `admin_contract` (astra-runtime) + settings JSON contract (`astra-core` `settings_contract_tests`) |

## Typical Workflows

### Daily Development

```bash
# Morning: begin from a deterministic provider boundary
make dev-start
make dev-status

# Need local workspace tools from Web
make dev-start-server-edge

# Development loop
# ... edit code ...
make dev-api-restart-debug
make test-contract      # Smallest relevant loop for API work
make test-offline       # Broader pre-PR gate

# Evening
make dev-stop           # Stop everything
```

### Testing Changes

```bash
# Fast focused contracts while iterating
make test-contract

# Full offline gate before the PR
make test-offline

# Online suite only when the changed boundary requires live dependencies
make test-online
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

- Generates `ASTRA_TOKEN_ENCRYPTION_KEY` if missing or still using a template placeholder
- Generates `ASTRA_JWT_SECRET` if missing or still using a template placeholder
- Generates `ASTRA_RUNTIME_ROOT_SECRET` if missing or still using a template placeholder
- Generates `MEMORIA_MASTER_KEY` if missing or still using a template placeholder

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
