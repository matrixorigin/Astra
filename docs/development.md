# Development Guide

## Prerequisites

- Python 3.10+
- Docker and Docker Compose
- Poetry (Python dependency manager)

## Initial Setup

### 1. Install Poetry (if not already installed)

```bash
curl -sSL https://install.python-poetry.org | python3 -
```

### 2. Create Virtual Environment

Poetry will automatically create a virtual environment when you run:

```bash
poetry install
```

Or manually create one:

```bash
# Option A: Let Poetry manage it (recommended)
poetry shell  # Activates the virtual environment

# Option B: Use Python venv
python3 -m venv .venv
source .venv/bin/activate  # On Linux/Mac
# .venv\Scripts\activate   # On Windows
pip install -e .
```

### 3. Setup Project

```bash
make setup
```

This will:
- Copy `.env.example` to `.env`
- Install Python dependencies via Poetry
- Prompt you to review `.env` configuration

### 4. Start Development Environment

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
