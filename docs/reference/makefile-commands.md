# Makefile Commands Reference

Complete reference for all available make commands.

## Quick Reference

| Command | Description |
|---------|-------------|
| `make help` | Show all available commands |
| `make setup` | Copy .env.example to .env |
| `make install-dev-deps` | Install all dependencies (runtime + dev + test) |
| `make install-check-deps` | Install check dependencies (lint + type-check, lighter) |
| `make dev-start` | Start all services |
| `make dev-stop` | Stop all services |
| `make dev-status` | Check service status |
| `make dev-test` | Run all tests |

## Command Categories

### 🚀 Composite Commands (Most Used)

| Command | Description |
|---------|-------------|
| `make dev-start` | Start all services (dependencies + API) |
| `make dev-stop` | Stop all services |
| `make dev-restart` | Restart all services |
| `make dev-status` | Show status of all services |
| `make dev-init` | Initialize environment (generate keys, fix config) |
| `make dev-clean` | Clean all data (with confirmation) |
| `make dev-reset` | Reset everything (clean + init + start) |

### 🐳 Dependency Services (MatrixOne + Redis)

| Command | Description |
|---------|-------------|
| `make dev-deps-up` | Start MatrixOne and Redis |
| `make dev-deps-down` | Stop dependency services |
| `make dev-deps-clean` | Stop and remove all data |
| `make dev-deps-status` | Show dependency service status |
| `make dev-deps-logs` | View dependency logs |
| `make dev-deps-wait` | Wait for services to be ready (max 15s) |

### 🔧 API Server (Source Mode)

| Command | Description |
|---------|-------------|
| `make dev-api-start` | Start API server from source |
| `make dev-api-stop` | Stop API server |
| `make dev-api-restart` | Restart API server |
| `make dev-api-logs` | View API server logs |
| `make dev-api-status` | Check API server status |

### 🐋 API Server (Docker Mode)

| Command | Description |
|---------|-------------|
| `make dev-api-docker-build` | Build API Docker image |
| `make dev-api-docker-up` | Start API in Docker |
| `make dev-api-docker-down` | Stop API Docker container |
| `make dev-api-docker-logs` | View API Docker logs |
| `make dev-api-docker-scale` | Scale API containers (REPLICAS=N) |

### 🧪 Testing

| Command | Description |
|---------|-------------|
| `make dev-test` | Run all tests (stops services after) |
| `make dev-test-keep` | Run all tests (keeps services running) |
| `make dev-test-unit` | Run unit tests only |
| `make dev-test-integration` | Run integration tests only |
| `make test` | Run all tests (alias) |
| `make test-unit` | Run unit tests (alias) |
| `make test-integration` | Run integration tests (alias) |

### 📦 Setup and Installation

| Command | Description |
|---------|-------------|
| `make setup` | Copy .env.example to .env (one-time) |
| `make install-dev-deps` | Install all dependencies (runtime + dev + test) |
| `make install-check-deps` | Install check dependencies (lint + type-check, lighter) |
| `make install` | Install package in development mode |
| `make clean` | Remove build artifacts |

### ✅ Code Quality

| Command | Description |
|---------|-------------|
| `make check` | Run all static checks (lint + type-check) |
| `make lint` | Run ruff linter |
| `make lint-fix` | Auto-fix linting issues |
| `make type-check` | Run mypy type checker |
| `make format` | Format code with ruff |

### 🗄️ Database Management

| Command | Description |
|---------|-------------|
| `make db-init` | Initialize database schema |
| `make db-migrate` | Run database migrations |
| `make db-reset` | Reset database (drop + recreate) |
| `make db-shell` | Open database shell |

### 🔄 Legacy Commands (Deprecated)

These commands still work but show deprecation warnings:

| Old Command | New Command | Status |
|-------------|-------------|--------|
| `make dev-up` | `make dev-deps-up` | ⚠️ Deprecated |
| `make dev-down` | `make dev-deps-down` | ⚠️ Deprecated |
| `make dev-logs` | `make dev-deps-logs` | ⚠️ Deprecated |

## Detailed Command Reference

### setup

Copy `.env.example` to `.env` (one-time setup).

```bash
make setup
```

**What it does:**
- Checks if `.env` exists
- If not, copies `.env.example` to `.env`
- Prompts user to review and customize

**Use when:**
- First time setup (manual approach)
- Need to reset environment configuration

**Note:** This does NOT install dependencies. Use `make install-dev-deps` or `make dev-init` for complete setup.

### install-dev-deps

Install all Python dependencies (runtime + dev + test).

```bash
make install-dev-deps
```

**What it does:**
- Runs `poetry install --with dev -E local-embedding`
- All dependencies are defined in `pyproject.toml` (single source of truth)

**Use when:**
- First time setup
- After pulling dependency changes
- Dependency installation issues

**Note:** For complete initialization, use `make dev-init` instead.

### install-check-deps

Install dependencies for static checks (lint, type-check) — skips `sentence-transformers`.

```bash
make install-check-deps
```

**What it does:**
- Runs `poetry install --with dev` (no `-E local-embedding`)
- Faster install, smaller footprint

**Use when:**
- CI static-check jobs (lint, mypy)
- Local linting without full test dependencies

### dev-start

Start all services (dependencies + API).

```bash
make dev-start
```

**What it does:**
1. Starts MatrixOne and Redis
2. Starts API server from source
3. Shows service status

**Time:** < 10 seconds

**Use when:**
- Starting daily development work
- After system reboot
- After `make dev-stop`

### dev-stop

Stop all services.

```bash
make dev-stop
```

**What it does:**
1. Stops API server
2. Stops MatrixOne and Redis

**Use when:**
- Ending development session
- Before system shutdown
- Before `make dev-clean`

### dev-status

Check status of all services.

```bash
make dev-status
```

**Output:**
```
=== Dependency Services ===
CONTAINER ID   IMAGE                STATUS
abc123         matrixorigin/...     Up 2 hours (healthy)
def456         redis:7-alpine       Up 2 hours

=== API Server ===
API server is running (PID: 12345)
Health check: {"status":"healthy","database":"connected"}
```

### dev-init

Initialize development environment.

```bash
make dev-init
```

**What it does:**
1. Copies `.env.example` to `.env` (if not exists)
2. Installs all dependencies (runtime + dev + test)
3. Generates `TOKEN_ENCRYPTION_KEY` if missing
4. Fixes `OPENAI_AKI_KEY` → `OPENAI_API_KEY`
5. Validates LLM provider/model configuration

**Use when:**
- First time setup
- After cloning repository
- After configuration errors

**Note:** This is the recommended way to set up the development environment.

### dev-clean

Clean all data (with confirmation).

```bash
make dev-clean
```

**What it does:**
1. Prompts for confirmation
2. Stops all services
3. Removes all Docker volumes
4. Deletes database data

**⚠️ Warning:** This deletes all data!

**Use when:**
- Need fresh start
- Database corruption
- Testing initialization

### dev-api-restart

Restart API server (keeps dependencies running).

```bash
make dev-api-restart
```

**What it does:**
1. Stops API server
2. Starts API server
3. Shows health check

**Time:** < 2 seconds

**Use when:**
- After code changes
- After configuration changes
- API server issues

### dev-test-keep

Run all tests, keep services running.

```bash
make dev-test-keep
```

**What it does:**
1. Ensures services are running
2. Runs pytest with all tests
3. Keeps services running after tests

**Use when:**
- Running tests during development
- Need to run tests multiple times
- Debugging test failures

### dev-deps-wait

Wait for dependency services to be ready.

```bash
make dev-deps-wait
```

**What it does:**
1. Waits for MatrixOne to accept connections (max 15s)
2. Waits for Redis to respond to PING (max 15s)
3. Shows error if timeout

**Use when:**
- After `make dev-deps-up`
- Before running tests
- Before starting API

### dev-api-docker-scale

Scale API containers in Docker mode.

```bash
make dev-api-docker-scale REPLICAS=3
```

**What it does:**
1. Scales API containers to specified number
2. Load balances across containers

**Use when:**
- Testing load balancing
- Simulating production
- Performance testing

## Environment Variables

Some commands accept environment variables:

```bash
# Scale API containers
make dev-api-docker-scale REPLICAS=5

# Run specific test
make dev-test ARGS="-k test_auth"

# Change API port
API_PORT=8001 make dev-api-start
```

## Tips and Tricks

### Fast Iteration

```bash
# Start once
make dev-start

# Make changes, restart API only
make dev-api-restart

# Run tests without stopping services
make dev-test-keep
```

### Debugging

```bash
# View logs in real-time
make dev-api-logs
make dev-deps-logs

# Check service status
make dev-status

# Check API health
curl http://localhost:8000/health
```

### Clean Slate

```bash
# Complete reset
make dev-clean
make dev-init
make dev-start
```

### Running Tests

```bash
# All tests, keep services running
make dev-test-keep

# Unit tests only
make dev-test-unit

# Specific test
make dev-test ARGS="-k test_auth"

# With coverage
make dev-test ARGS="--cov=core"
```

## See Also

- [Development Workflow Guide](../guides/development-workflow.md) - Detailed workflows
- [Configuration Reference](configuration.md) - Environment variables
- [Troubleshooting Guide](../guides/troubleshooting.md) - Common issues
