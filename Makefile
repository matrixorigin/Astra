# mo-agent-engine Makefile
# Inspired by MatrixOne's development workflow

.PHONY: help
help:
	@echo "mo-agent-engine Development Commands"
	@echo "=================================="
	@echo ""
	@echo "Environment Setup:"
	@echo "  make setup              - Initial project setup (copy .env, install deps)"
	@echo "  make install            - Install Python dependencies"
	@echo "  make install-runtime    - Install runtime dependencies (Docker, Firecracker)"
	@echo "  make check-runtime      - Check runtime environment"
	@echo "  make lock               - Update dependency lock file (poetry.lock)"
	@echo ""
	@echo "Development Environment:"
	@echo "  make dev-up             - Start infra (MatrixOne + Redis)"
	@echo "  make dev-down           - Stop all services"
	@echo "  make dev-full           - Start everything in containers (build + run)"
	@echo "  make dev-restart        - Restart all services"
	@echo "  make dev-logs           - Show all logs (tail -f)"
	@echo "  make dev-logs-db        - Show MatrixOne logs"
	@echo "  make dev-ps             - Show service status"
	@echo "  make dev-clean          - Stop and remove all data (WARNING: destructive!)"
	@echo ""
	@echo "Database:"
	@echo "  make db-init            - Initialize database schema"
	@echo "  make db-init-agent      - Initialize agent configuration system (RBAC + tables)"
	@echo "  make db-connect         - Connect to MatrixOne CLI"
	@echo "  make db-reset           - Reset database (drop + recreate)"
	@echo ""
	@echo "Testing:"
	@echo "  make test               - Run all tests"
	@echo "  make test-unit          - Run unit tests"
	@echo "  make test-integration   - Run integration tests"
	@echo "  make test-api          - Run API integration tests"
	@echo "  make test-e2e           - Run end-to-end tests"
	@echo "  make test-runtime       - Run runtime tests (Docker, Firecracker)"
	@echo ""
	@echo "Code Quality:"
	@echo "  make check              - Run all static checks (lint + format + type-check)"
	@echo "  make ci                 - Run all CI checks (check + test)"
	@echo "  make lint               - Run linters (ruff)"
	@echo "  make lint-fix           - Run linters with auto-fix"
	@echo "  make type-check         - Run type checker (mypy)"
	@echo "  make format             - Format code"
	@echo "  make format-check       - Check code formatting"
	@echo ""
	@echo "Examples:"
	@echo "  make setup && make dev-up                     # First time setup"
	@echo "  make dev-up && make test                      # Daily development"
	@echo "  make dev-full                                 # All services + GPU + model"

# ============================================================================
# Environment Setup
# ============================================================================

.PHONY: setup
setup:
	@echo "Setting up mo-agent-engine development environment..."
	@if [ ! -f .env ]; then \
		cp .env.example .env; \
		echo "✅ Created .env file (please review and customize)"; \
	else \
		echo "⚠️  .env already exists, skipping"; \
	fi
	@echo ""
	@$(MAKE) install
	@echo ""
	@echo "✅ Setup complete! Next steps:"
	@echo "   1. Review and customize .env file"
	@echo "   2. Run: make dev-up"
	@echo "   3. Run: make db-init"

.PHONY: install
install:
	@echo "Installing Python dependencies..."
	@if command -v poetry >/dev/null 2>&1; then \
		poetry install; \
	else \
		pip install -e .; \
	fi
	@echo "✅ Python dependencies installed"

.PHONY: install-runtime
install-runtime:
	@echo "Installing runtime dependencies..."
	@echo ""
	@echo "Checking Docker..."
	@if command -v docker >/dev/null 2>&1; then \
		echo "✅ Docker installed: $$(docker --version)"; \
	else \
		echo "❌ Docker not found"; \
		echo "   Install: https://docs.docker.com/get-docker/"; \
	fi
	@echo ""
	@echo "Checking Firecracker environment..."
	@if [ "$$(uname)" != "Linux" ]; then \
		echo "⚠️  Firecracker only supports Linux (current: $$(uname))"; \
		echo "   Skipping Firecracker installation"; \
	elif ! grep -qE "vmx|svm" /proc/cpuinfo; then \
		echo "❌ CPU doesn't support virtualization"; \
		echo "   Firecracker requires Intel VT-x or AMD-V"; \
	elif [ ! -e /dev/kvm ]; then \
		echo "❌ /dev/kvm not found"; \
		echo "   Install KVM: sudo apt install qemu-kvm (Ubuntu/Debian)"; \
	else \
		echo "✅ KVM device available"; \
		if groups | grep -q kvm; then \
			echo "✅ User in kvm group"; \
		else \
			echo "⚠️  User not in kvm group"; \
			echo "   Run: sudo usermod -aG kvm $$USER"; \
			echo "   Then: newgrp kvm (or re-login)"; \
		fi; \
		if command -v firecracker >/dev/null 2>&1; then \
			echo "✅ Firecracker installed: $$(firecracker --version 2>&1 | head -1)"; \
		else \
			echo "⚠️  Firecracker not installed"; \
			echo "   Installing Firecracker..."; \
			bash -c 'set -e; \
				ARCH=$$(uname -m); \
				RELEASE_URL="https://github.com/firecracker-microvm/firecracker/releases"; \
				LATEST=$$(basename $$(curl -fsSLI -o /dev/null -w %{url_effective} $${RELEASE_URL}/latest)); \
				echo "   Downloading $${LATEST}..."; \
				curl -sL $${RELEASE_URL}/download/$${LATEST}/firecracker-$${LATEST}-$${ARCH}.tgz | tar -xz; \
				sudo mv release-$${LATEST}-$${ARCH}/firecracker-$${LATEST}-$${ARCH} /usr/local/bin/firecracker; \
				sudo chmod +x /usr/local/bin/firecracker; \
				rm -rf release-$${LATEST}-$${ARCH}; \
				echo "✅ Firecracker installed: $$(firecracker --version 2>&1 | head -1)"'; \
		fi; \
	fi
	@echo ""
	@echo "✅ Runtime dependency check complete"
	@echo "   Run 'make check-runtime' to verify"

.PHONY: check-runtime
check-runtime:
	@echo "Checking runtime environment..."
	@echo ""
	@echo "1. Docker:"
	@if command -v docker >/dev/null 2>&1; then \
		echo "   ✅ $$(docker --version)"; \
		if docker ps >/dev/null 2>&1; then \
			echo "   ✅ Docker daemon running"; \
		else \
			echo "   ❌ Docker daemon not running"; \
		fi; \
	else \
		echo "   ❌ Not installed"; \
	fi
	@echo ""
	@echo "2. Firecracker:"
	@if [ "$$(uname)" != "Linux" ]; then \
		echo "   ⚠️  Not supported on $$(uname)"; \
	elif command -v firecracker >/dev/null 2>&1; then \
		echo "   ✅ $$(firecracker --version 2>&1 | head -1)"; \
		if [ -e /dev/kvm ]; then \
			echo "   ✅ /dev/kvm exists"; \
			if groups | grep -q kvm; then \
				echo "   ✅ User in kvm group"; \
			else \
				echo "   ❌ User not in kvm group"; \
			fi; \
		else \
			echo "   ❌ /dev/kvm not found"; \
		fi; \
	else \
		echo "   ❌ Not installed"; \
	fi
	@echo ""
	@echo "3. Python runtime module:"
	@python3 -c "from core.runtime import create_runtime; print('   ✅ Runtime module OK')" 2>/dev/null || echo "   ❌ Runtime module error"

.PHONY: lock
lock:
	@echo "Updating dependency lock file..."
	@if command -v poetry >/dev/null 2>&1; then \
		poetry lock; \
		echo "✅ poetry.lock updated"; \
	else \
		echo "⚠️  Poetry not found, skipping lock (pip doesn't use lock files)"; \
	fi

# ============================================================================
# Development Environment
# ============================================================================

.PHONY: dev-up
dev-up:
	@echo "Starting mo-agent-engine development environment..."
	@cd deployment/all-in-one && docker compose up -d
	@echo ""
	@echo "✅ Services started!"
	@echo "   MatrixOne: mysql -h127.0.0.1 -P6001 -uroot -p111"
	@echo "   Redis:     redis-cli -h 127.0.0.1 -p 6379"
	@echo ""
	@echo "Next: mo-admin init && mo-agent chat"

.PHONY: dev-down
dev-down:
	@echo "Stopping mo-agent-engine services..."
	@cd deployment/all-in-one && docker compose down

.PHONY: dev-restart
dev-restart:
	@echo "Restarting mo-agent-engine services..."
	@cd deployment/all-in-one && docker compose restart

.PHONY: dev-logs
dev-logs:
	@cd deployment/all-in-one && docker compose logs -f

.PHONY: dev-logs-db
dev-logs-db:
	@cd deployment/all-in-one && docker compose logs -f matrixone

.PHONY: dev-ps
dev-ps:
	@cd deployment/all-in-one && docker compose ps

.PHONY: dev-clean
dev-clean:
	@echo "⚠️  WARNING: This will delete all data!"
	@printf "Are you sure? [y/N] "; \
	read REPLY; \
	if [ "$$REPLY" = "y" ] || [ "$$REPLY" = "Y" ]; then \
		cd deployment/all-in-one && docker compose --profile full down -v; \
		echo "✅ All data removed"; \
	else \
		echo "Cancelled"; \
	fi

.PHONY: dev-full
dev-full:
	@echo "Starting all services in containers (build + run)..."
	@cd deployment/all-in-one && docker compose --profile full up -d --build

.PHONY: dev-init
dev-init: dev-up
	@echo "✅ Development environment ready!"

# ============================================================================
# Database
# ============================================================================

.PHONY: db-init
db-init:
	@echo "Database tables are auto-initialized by FastAPI on startup"
	@echo "No manual initialization needed"

.PHONY: db-init-agent
db-init-agent:
	@echo "Initializing agent configuration system..."
	@python3 scripts/init_agent_system.py

.PHONY: db-connect
db-connect:
	@echo "Connecting to MatrixOne..."
	@echo ""
	@mysql -h127.0.0.1 -P6001 -uroot -p111 2>/dev/null || \
	mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl 2>/dev/null || \
	mysql -h127.0.0.1 -P6001 -uroot -p111 --skip_ssl 2>/dev/null || \
	(echo "❌ Connection failed. Please try manually:" && \
	 echo "   mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl   # or" && \
	 echo "   mysql -h127.0.0.1 -P6001 -uroot -p111 --skip_ssl   # (underscore)" && \
	 exit 1)

.PHONY: db-reset
db-reset:
	@echo "⚠️  WARNING: This will drop and recreate the database!"
	@printf "Are you sure? [y/N] "; \
	read REPLY; \
	if [ "$$REPLY" = "y" ] || [ "$$REPLY" = "Y" ]; then \
		if [ -f .env ]; then \
			export $$(cat .env | grep -v '^#' | xargs); \
			DB_NAME=$${MATRIXONE_DATABASE:-dev_agent}; \
		else \
			DB_NAME=dev_agent; \
		fi; \
		mysql -h127.0.0.1 -P6001 -uroot -p111 -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
		mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
		mysql -h127.0.0.1 -P6001 -uroot -p111 --skip_ssl -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;"; \
		$(MAKE) db-init; \
		echo "✅ Database reset complete"; \
	else \
		echo "Cancelled"; \
	fi

# ============================================================================
# Testing
# ============================================================================

.PHONY: test
test:
	@echo "Running all tests..."
	@python -m pytest tests/ -v

.PHONY: test-parallel
test-parallel:
	@echo "Running all tests in parallel..."
	@python -m pytest tests/ -n auto -v

.PHONY: test-unit
test-unit:
	@echo "Running unit tests..."
	@python -m pytest tests/unit/ -v

.PHONY: test-unit-parallel
test-unit-parallel:
	@echo "Running unit tests in parallel..."
	@python -m pytest tests/unit/ -n auto -v

.PHONY: test-integration
test-integration:
	@echo "Running integration tests..."
	@python -m pytest tests/integration/ -v

.PHONY: test-integration-parallel
test-integration-parallel:
	@echo "Running integration tests in parallel..."
	@python -m pytest tests/integration/ -n auto -v

.PHONY: test-cleanup
test-cleanup:
	@echo "Cleaning up test databases..."
	@python scripts/cleanup_test_dbs.py

.PHONY: test-api
test-api:
	@echo "Running API integration tests..."
	@python -m pytest tests/integration/api/ -v

.PHONY: test-e2e
test-e2e:
	@echo "Running end-to-end tests..."
	@python -m pytest tests/e2e/ -v

.PHONY: test-runtime
test-runtime:
	@echo "Running runtime tests..."
	@echo ""
	@echo "Testing Docker runtime..."
	@pytest tests/unit/test_docker_runtime.py -v
	@echo ""
	@echo "Testing Firecracker runtime..."
	@pytest tests/unit/test_firecracker_runtime.py -v
	@echo ""
	@echo "Testing Subprocess runtime..."
	@pytest tests/unit/test_subprocess_runtime.py -v 2>/dev/null || echo "⚠️  Subprocess runtime tests not found"
	@echo ""
	@echo "✅ Runtime tests complete"

# ============================================================================
# Code Quality
# ============================================================================

# Check if poetry environment is set up
.PHONY: check-env
check-env:
	@poetry --version >/dev/null 2>&1 || (echo "❌ Error: poetry not found. Install it first: https://python-poetry.org/docs/#installation" && exit 1)
	@poetry run python --version >/dev/null 2>&1 || (echo "❌ Error: Poetry environment not set up. Run 'make install' first." && exit 1)

.PHONY: check
check: check-env lint format-check type-check
	@echo "✅ All static checks passed!"

.PHONY: lint
lint:
	@echo "Running linters..."
	@poetry run ruff check .

.PHONY: lint-fix
lint-fix:
	@echo "Running linters with auto-fix..."
	@poetry run ruff check --fix .

.PHONY: type-check
type-check:
	@echo "Running type checker..."
	@poetry run mypy sdk/ core/ api/

.PHONY: format
format:
	@echo "Formatting code..."
	@poetry run ruff format .

.PHONY: format-check
format-check:
	@echo "Checking code formatting..."
	@poetry run ruff format --check .

.PHONY: pre-commit
.PHONY: ci
ci: check test
	@echo "✅ All CI checks passed!"
