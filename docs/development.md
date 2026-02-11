# Development Guide

## Quick Start

```bash
# 1. Setup environment
make setup

# 2. Start services
make dev-up

# 3. Initialize database
make db-init

# 4. Run tests
make test

# 5. Start API (optional)
python api/main.py
```

## Prerequisites

- Python 3.10+
- Docker and Docker Compose
- Make

---

## Development Setup

### 1. Create Virtual Environment

```bash
conda create -n dev-agent python=3.11
conda activate dev-agent
```

### 2. Install Dependencies

```bash
make setup
# or
pip install -e .
```

**New dependencies** (production features):
- `PyGithub` - GitHub API client
- `prometheus-client` - Metrics
- `PyJWT` - Authentication
- `vcrpy` - API testing (dev only)

---

## Testing

### Run All Tests
```bash
make test
```

### Test Categories

**1. Unit Tests** (fast):
```bash
pytest tests/unit/
```

**2. Integration Tests** (mock GitHub):
```bash
pytest tests/integration/test_skills.py
```

**3. VCR Tests** (recorded real API):
```bash
# First run (needs GITHUB_TOKEN)
export GITHUB_TOKEN=ghp_your_token
pytest tests/integration/test_github_real.py

# Subsequent runs (uses cassettes)
pytest tests/integration/test_github_real.py
```

**4. E2E Tests** (replay):
```bash
pytest tests/integration/test_replay_e2e.py
```

### Re-record GitHub API Responses
```bash
rm fixtures/vcr_cassettes/*.yaml
export GITHUB_TOKEN=ghp_your_token
pytest tests/integration/test_github_real.py
```

---

## Production Features

### Configuration

**Environment files**:
- `.env` - Development (default)
- `.env.production` - Production

**Load config**:
```python
from core.config import get_settings
settings = get_settings()
```

### Logging

**Structured JSON logs**:
```python
from core.logging_config import setup_logging, get_logger

setup_logging(level="INFO", json_format=True)
logger = get_logger(__name__)
logger.info("Message", extra={"user_id": "alice"})
```

### Authentication

**API Key**:
```bash
curl -H "X-API-Key: your-key" http://localhost:8000/api/protected
```

**JWT**:
```bash
# Get token
curl -X POST "http://localhost:8000/api/token?user_id=alice"

# Use token
curl -H "Authorization: Bearer <token>" http://localhost:8000/api/protected
```

### Monitoring

**Prometheus metrics**:
```bash
curl http://localhost:8000/metrics
```

**Health checks**:
```bash
curl http://localhost:8000/health
curl http://localhost:8000/health/ready
```

---

## Deployment

### Docker

**Build**:
```bash
docker build -t mo-agent-engine:latest .
```

**Run**:
```bash
docker run -p 8000:8000 --env-file .env.production mo-agent-engine:latest
```

### Docker Compose

**Production**:
```bash
docker-compose -f docker-compose.prod.yml up -d
```

**Verify**:
```bash
curl http://localhost:8000/health
```

---

## Architecture

See design documents in `docs/design/`:
- `skills-first-architecture.md` - Core architecture
- `github-integration.md` - GitHub integration
- `llm-integration.md` - LLM integration

---

### 2. Setup Project

```bash
make setup
```

This will:
- Copy `.env.example` to `.env`
- Install Python dependencies
- Prompt you to review `.env` configuration

### 3. Start Development Environment

```bash
# Start MatrixOne + Redis
make dev-up

# Initialize database schema
make db-init

# Verify connection
make db-connect
```

## Daily Development Workflow

```bash
# Activate virtual environment (if using Poetry)
poetry shell

# Or if using venv
source .venv/bin/activate

# Start services
make dev-up

# Run tests
make test

# Stop services when done
make dev-down
```

## Troubleshooting

### Port Conflicts

If `make dev-up` fails with port conflicts, check `.env` file:

```bash
# Default ports
MATRIXONE_PORT=6001  # Change if 6001 is occupied
REDIS_PORT=6379      # Change if 6379 is occupied
```

## Troubleshooting

### Database Connection Issues

If `make db-connect` fails with SSL errors:

```bash
# Try manually with --skip-ssl
mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl

# Or configure your MySQL client
# Add to ~/.my.cnf:
[client]
skip-ssl
```

**Note**: The `init-db.sh` script automatically handles SSL issues by trying both with and without SSL.

### Virtual Environment Issues

If dependencies are not found:

```bash
# Ensure you're in the virtual environment
poetry shell

# Or activate venv
source .venv/bin/activate

# Reinstall dependencies
poetry install
```

## Available Commands

Run `make help` to see all available commands.
