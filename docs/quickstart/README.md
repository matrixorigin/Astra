# 5-Minute Quick Start

Get mo-agent running in under 5 minutes.

## Prerequisites

- Python 3.11+
- Docker and Docker Compose
- Conda (recommended) or venv

## Quick Start

### Option 1: Development Mode (Recommended for Development)

```bash
# 1. Clone and setup
git clone https://github.com/matrixorigin/mo-agent.git
cd mo-agent
conda create -n dev-agent python=3.11
conda activate dev-agent

# 2. Initialize and start (< 10 seconds)
make dev-init          # Auto-generate keys, install deps, fix config
make dev-start         # Start all services

# 3. Check status
make dev-status

# 4. Visit API
open http://localhost:8000/docs
```

**That's it!** You now have:
- ✅ MatrixOne database running
- ✅ Redis cache running
- ✅ API server running on port 8000
- ✅ Interactive API docs at http://localhost:8000/docs

### Option 2: Docker Mode (Recommended for Production)

```bash
# 1. Clone and configure
git clone https://github.com/matrixorigin/mo-agent.git
cd mo-agent
cp .env.example .env
# Edit .env: set TOKEN_ENCRYPTION_KEY, JWT_SECRET_KEY, LLM tokens

# 2. Start everything
make dev-start-docker

# 3. Visit API
open http://localhost:8000/docs
```

## Next Steps

### Try the API

```bash
# Health check
curl http://localhost:8000/health

# Register a user
curl -X POST http://localhost:8000/auth/register \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"secret123","email":"alice@example.com"}'

# Login
curl -X POST http://localhost:8000/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"secret123"}'
```

### Use the CLI

```bash
# Interactive chat
mo-agent chat

# View models
mo-agent model list

# Health check
mo-agent health
```

### Daily Development Workflow

```bash
# Start working
make dev-start

# Make code changes...

# Restart API to apply changes
make dev-api-restart

# Run tests
make dev-test-keep

# Stop everything
make dev-stop
```

## Common Commands

| Command | Description |
|---------|-------------|
| `make dev-start` | Start all services |
| `make dev-stop` | Stop all services |
| `make dev-status` | Check service status |
| `make dev-api-restart` | Restart API after code changes |
| `make dev-test` | Run all tests |
| `make dev-clean` | Clean all data (with confirmation) |
| `make help` | Show all available commands |

## Troubleshooting

### Services not starting?

```bash
# Check Docker services
make dev-deps-status

# View logs
make dev-deps-logs

# Restart dependencies
make dev-deps-down
make dev-deps-up
```

### API server issues?

```bash
# Check API logs
make dev-api-logs

# Restart API
make dev-api-restart
```

### Need to reset everything?

```bash
# Clean and restart
make dev-clean
make dev-init
make dev-start
```

## Learn More

- [Development Workflow Guide](../guides/development-workflow.md) - Detailed development workflows
- [API Reference](../reference/api-reference.md) - Complete API documentation
- [Configuration Guide](../reference/configuration.md) - Environment variables and settings
- [Troubleshooting Guide](../guides/troubleshooting.md) - Common issues and solutions
