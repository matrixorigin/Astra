# Development Guide

## Quick Start

```bash
make setup      # Install dependencies, copy .env.example → .env
make dev-up     # Start MatrixOne + Redis (Docker)
make test       # Run all tests (527 tests, DB auto-initializes)
make dev-down   # Stop services
```

## Prerequisites

- Python 3.11+
- Docker and Docker Compose
- Make

## Setup

```bash
conda create -n dev-agent python=3.11
conda activate dev-agent
make setup
```

## Testing

```bash
make test                              # All tests
pytest tests/unit/                     # Unit tests only (~300)
pytest tests/integration/              # Integration tests (~200, needs DB)
```

### Code Quality

```bash
make check       # All checks (lint + type-check)
make lint        # Ruff linter
make lint-fix    # Auto-fix
make type-check  # Mypy
make format      # Code formatting
```

## Running the API

```bash
# Development (auto-reload)
uvicorn api.main:app --reload --port 8000

# Interactive docs
open http://localhost:8000/docs
```

## Configuration

Environment variables in `.env`:

```bash
MATRIXONE_HOST=localhost
MATRIXONE_PORT=6001
MATRIXONE_USER=root
MATRIXONE_PASSWORD=111
MATRIXONE_DATABASE=dev_agent
REDIS_URL=redis://localhost:6379
JWT_SECRET=your-secret
```

Database tables auto-initialize on first API start.

## Monitoring

```bash
curl http://localhost:8000/health        # Health check
curl http://localhost:8000/health/ready   # Readiness
curl http://localhost:8000/metrics        # Prometheus metrics
```

## Docker

```bash
docker build -t mo-agent-engine .
docker-compose -f docker-compose.prod.yml up -d
```

## Troubleshooting

**Port conflicts**: Change `MATRIXONE_PORT` or `REDIS_PORT` in `.env`.

**DB connection fails**: Try `mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl`.

**Dependencies not found**: Ensure virtual environment is activated.

## Documentation

- [Architecture](design/ARCHITECTURE.md) — system design
- [API Reference](api-reference.md) — endpoint documentation
- [Implementation](implementation/) — component details
