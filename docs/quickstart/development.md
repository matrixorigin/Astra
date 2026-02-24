# Development Environment Setup

Complete guide for setting up mo-agent development environment.

## Prerequisites

- **Python**: 3.11 or higher
- **Docker**: Latest version with Docker Compose
- **Conda**: Recommended for Python environment management
- **Make**: For running development commands
- **Git**: For version control

## Installation

### 1. Clone Repository

```bash
git clone https://github.com/matrixorigin/mo-agent.git
cd mo-agent
```

### 2. Create Python Environment

**Using Conda (Recommended):**
```bash
conda create -n dev-agent python=3.11
conda activate dev-agent
```

**Using venv:**
```bash
python3.11 -m venv .venv
source .venv/bin/activate  # On Windows: .venv\Scripts\activate
```

### 3. Install Dependencies

```bash
make setup
```

This will:
- Install Python dependencies from `pyproject.toml`
- Install development tools (pytest, ruff, mypy)
- Set up pre-commit hooks (optional)

### 4. Configure Environment

```bash
# Initialize environment (auto-generates keys)
make dev-init
```

This automatically:
- Generates `TOKEN_ENCRYPTION_KEY` if missing
- Fixes common configuration errors (e.g., `OPENAI_AKI_KEY` → `OPENAI_API_KEY`)
- Validates LLM provider/model configuration

**Manual configuration (optional):**
```bash
cp .env.example .env
# Edit .env with your settings
```

Required environment variables:
- `TOKEN_ENCRYPTION_KEY` - For encrypting API tokens (auto-generated)
- `JWT_SECRET_KEY` - For JWT authentication (auto-generated)
- `OPENAI_API_KEY` - Your OpenAI API key (if using OpenAI)
- `LLM_PROVIDER` - LLM provider (openai, anthropic, etc.)
- `LLM_MODEL` - Model name (gpt-4, claude-3-opus, etc.)

### 5. Start Services

```bash
# Start all services (< 10 seconds)
make dev-start
```

This starts:
- **MatrixOne** - Database (port 6001)
- **Redis** - Cache (port 6379)
- **API Server** - REST API (port 8000)

### 6. Verify Installation

```bash
# Check service status
make dev-status

# Test API
curl http://localhost:8000/health

# Expected response:
# {"status":"healthy","database":"connected"}
```

## Development Workflow

### Daily Workflow

```bash
# 1. Start services
make dev-start

# 2. Make code changes...

# 3. Restart API to apply changes
make dev-api-restart

# 4. Run tests
make dev-test-keep

# 5. Stop services when done
make dev-stop
```

### Common Commands

| Command | Description |
|---------|-------------|
| `make dev-start` | Start all services |
| `make dev-stop` | Stop all services |
| `make dev-restart` | Restart all services |
| `make dev-status` | Check service status |
| `make dev-api-restart` | Restart API only |
| `make dev-api-logs` | View API logs |
| `make dev-deps-logs` | View dependency logs |
| `make dev-test` | Run tests (stops services after) |
| `make dev-test-keep` | Run tests (keeps services running) |
| `make dev-clean` | Clean all data |

### Code Quality

```bash
# Run all checks
make check

# Format code
make format

# Run linter
make lint

# Fix linting issues
make lint-fix

# Type checking
make type-check
```

## Using the CLI

### mo-agent (User CLI)

```bash
# Interactive chat
mo-agent chat --user-id alice

# Manage models
mo-agent model list
mo-agent model show gpt-4

# Manage skills
mo-agent skill list
mo-agent skill register skill.json

# Manage sessions
mo-agent session list
mo-agent session show <session_id>

# Replay conversations
mo-agent replay <session_id>

# Health check
mo-agent health
```

### mo-admin (Admin CLI)

```bash
# Initialize system
mo-admin init

# Manage models
mo-admin model add gpt-4 openai --scope global
mo-admin model list
mo-admin model remove gpt-4 --scope global

# Manage API tokens
mo-admin token create --type llm --provider openai --scope global
mo-admin token list

# View audit logs
mo-admin audit logs --user alice --since 2026-02-01
```

## Using the API

### Interactive Documentation

Visit http://localhost:8000/docs for Swagger UI with:
- Complete API reference
- Try-it-out functionality
- Request/response examples
- Authentication testing

Alternative: http://localhost:8000/redoc for ReDoc interface

### Example API Usage

```bash
# Register user
curl -X POST http://localhost:8000/auth/register \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"secret123","email":"alice@example.com"}'

# Login
curl -X POST http://localhost:8000/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"secret123"}'

# Use token in subsequent requests
curl -X GET http://localhost:8000/auth/me \
  -H "Authorization: Bearer <your_access_token>"
```

## Troubleshooting

### Services Won't Start

```bash
# Check Docker services
make dev-deps-status

# View logs
make dev-deps-logs

# Restart dependencies
make dev-deps-down
make dev-deps-up
```

### Database Connection Issues

```bash
# Check MatrixOne status
docker ps | grep matrixone

# View MatrixOne logs
make dev-deps-logs

# Wait for MatrixOne to be ready (max 15 seconds)
make dev-deps-wait
```

### API Server Issues

```bash
# Check if API is running
make dev-api-status

# View API logs
make dev-api-logs

# Restart API
make dev-api-restart
```

### Port Conflicts

If ports 6001, 6379, or 8000 are already in use:

```bash
# Find process using port
lsof -i :8000

# Kill process
kill -9 <PID>

# Or change ports in .env
API_PORT=8001
```

### Reset Everything

```bash
# Clean all data and restart
make dev-clean
make dev-init
make dev-start
```

## Next Steps

- [Development Workflow Guide](../guides/development-workflow.md) - Detailed workflows
- [Testing Guide](../guides/testing.md) - Writing and running tests
- [API Reference](../reference/api-reference.md) - Complete API documentation
- [Configuration Reference](../reference/configuration.md) - All configuration options
