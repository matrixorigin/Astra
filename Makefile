# mo-agent-engine Makefile
# Inspired by MatrixOne's development workflow

.PHONY: help
help:
	@echo "mo-agent-engine Development Commands"
	@echo "=================================="
	@echo ""
	@echo "Quick Start:"
	@echo "  make dev-start          - Start all (deps + API source mode) [MOST USED]"
	@echo "  make dev-start-docker   - Start all (deps + API Docker mode)"
	@echo "  make dev-stop           - Stop all services"
	@echo "  make dev-status         - Show all service status"
	@echo "  make dev-init           - Initialize development environment"
	@echo ""
	@echo "Dependency Services (MatrixOne + Redis):"
	@echo "  make dev-deps-up        - Start dependency services"
	@echo "  make dev-deps-down      - Stop dependency services"
	@echo "  make dev-deps-clean     - Stop and remove all data (WARNING: destructive!)"
	@echo "  make dev-deps-status    - Show dependency status"
	@echo "  make dev-deps-logs      - Show all dependency logs"
	@echo "  make dev-deps-logs-db   - Show MatrixOne logs only"
	@echo "  make dev-deps-logs-redis - Show Redis logs only"
	@echo "  make dev-db-connect     - Connect to MatrixOne CLI"
	@echo ""
	@echo "Logging:"
	@echo "  make dev-logs-clean     - Clear Docker logs"
	@echo ""
	@echo "API Server (Source Code Mode):"
	@echo "  make dev-api-start      - Start API server (hot reload)"
	@echo "  make dev-api-stop       - Stop API server"
	@echo "  make dev-api-restart    - Restart API server"
	@echo "  make dev-api-logs       - Show API server logs"
	@echo "  make dev-api-status     - Show API server status"
	@echo ""
	@echo "API Server (Docker Mode):"
	@echo "  make dev-api-docker-build - Build API server image"
	@echo "  make dev-api-docker-up  - Start API server container"
	@echo "  make dev-api-docker-down - Stop API server container"
	@echo "  make dev-api-docker-logs - Show container logs"
	@echo "  make dev-api-docker-scale REPLICAS=N - Scale API server"
	@echo ""
	@echo "Testing:"
	@echo "  make test               - Run all tests"
	@echo "  make test-unit          - Run unit tests only"
	@echo "  make test-integration   - Run integration tests only"
	@echo "  make verify             - E2E verification (real CLI + API + LLM)"
	@echo "  make verify-talk        - Talk verification (real CLI + API + LLM)"
	@echo ""
	@echo "Environment Setup:"
	@echo "  make dev-init           - Complete initialization (setup + deps + config)"
	@echo "  make dev-setup-demo     - Interactive demo setup (admin + model + user)"
	@echo "  make setup              - Copy .env.example → .env (one-time, no deps)"
	@echo "  make install-dev-deps   - Install all dependencies (runtime + dev + test)"
	@echo "  make install-check-deps - Install check dependencies (lint + type-check, lighter)"
	@echo ""
	@echo "Code Quality:"
	@echo "  make check              - Run all static checks"
	@echo "  make ci                 - Run all CI checks (check + test)"
	@echo "  make lint               - Run linters"
	@echo "  make lint-fix           - Auto-fix linting issues"
	@echo ""
	@echo "Memoria Lite (MCP Memory Server):"
	@echo "  make bump-memoria-version BUMP=patch  - Bump version (patch/minor/major)"
	@echo "  make build-memoria     - Build wheel distribution"
	@echo "  make publish-memoria   - Publish to PyPI"
	@echo "  make publish-memoria-test - Publish to TestPyPI"
	@echo ""
	@echo "Examples:"
	@echo "  make dev-start                    # Daily development"
	@echo "  make dev-api-restart              # After code changes"
	@echo "  make test                         # Run all tests"
	@echo "  make test-unit                    # Run unit tests only"
	@echo "  make dev-api-docker-scale REPLICAS=4  # Test load balancing"
	@echo "  make bump-memoria-version BUMP=minor # Bump 0.2.3 → 0.3.0"

# ============================================================================
# Environment Setup
# ============================================================================

.PHONY: dev-init
dev-init: setup install-dev-deps
	@echo "Initializing development environment..."
	@python3 scripts/dev/init.py
	@echo ""
	@echo "✅ Development environment initialized!"
	@echo ""
	@echo "Next: make dev-start"

.PHONY: dev-setup-demo
dev-setup-demo:
	@bash scripts/setup/demo-init.sh

.PHONY: setup
setup:
	@echo "Setting up mo-agent-engine development environment..."
	@if [ ! -f .env ]; then \
		cp .env.example .env; \
		echo "✅ Created .env file (please review and customize)"; \
	else \
		echo "⚠️  .env already exists, skipping"; \
	fi

.PHONY: install-dev-deps
install-dev-deps:
	@echo "Installing all dependencies (runtime + dev + test)..."
	@# pyproject.toml is the single source of truth for all dependencies.
	@# --with dev includes test deps (sentence-transformers, freezegun, etc.)
	@# -E local-embedding installs optional extras for full test coverage.
	@command -v poetry >/dev/null 2>&1 || { echo "❌ Poetry not found. Install: pip install poetry"; exit 1; }
	@poetry install --with dev -E local-embedding
	@echo "✅ All dependencies installed"

# Lighter install for static checks (lint, type-check) — skips sentence-transformers.
.PHONY: install-check-deps
install-check-deps:
	@echo "Installing check dependencies (runtime + dev, no extras)..."
	@command -v poetry >/dev/null 2>&1 || { echo "❌ Poetry not found. Install: pip install poetry"; exit 1; }
	@poetry install --with dev
	@echo "✅ Check dependencies installed"

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

# ============================================================================
# Development - Dependency Services (MatrixOne + Redis)
# ============================================================================

.PHONY: dev-deps-up
dev-deps-up:
	@echo "Starting dependency services (MatrixOne + Redis)..."
	@if [ -d deployment/all-in-one/data ] && [ "$$(stat -c '%u' deployment/all-in-one/data 2>/dev/null || stat -f '%u' deployment/all-in-one/data 2>/dev/null)" != "$$(id -u)" ]; then \
		echo "❌ Error: Data directory owned by root"; \
		echo "   Run: make dev-clean (to delete data)"; \
		echo "   Or:  sudo chown -R $$(id -u):$$(id -g) deployment/all-in-one/data (to fix permissions)"; \
		exit 1; \
	fi
	@mkdir -p deployment/all-in-one/data/matrixone deployment/all-in-one/data/matrixone/logs deployment/all-in-one/data/redis
	@cd deployment/all-in-one && UID=$$(id -u) GID=$$(id -g) docker compose up -d matrixone redis
	@echo "✅ Dependency services started"

.PHONY: dev-deps-down
dev-deps-down:
	@echo "Stopping dependency services..."
	@cd deployment/all-in-one && docker compose down
	@echo "✅ Dependency services stopped"

.PHONY: dev-deps-clean
dev-deps-clean:
	@echo "⚠️  WARNING: This will delete all dependency data!"
	@printf "Are you sure? [y/N] " && read REPLY && \
	if [ "$$REPLY" = "y" ] || [ "$$REPLY" = "Y" ]; then \
		cd deployment/all-in-one && docker compose down -v; \
		if [ -d data ]; then \
			if [ "$$(stat -c '%u' data 2>/dev/null || stat -f '%u' data 2>/dev/null)" != "$$(id -u)" ]; then \
				sudo rm -rf data; \
			else \
				rm -rf data; \
			fi; \
		fi; \
		cd ../.. && rm -f api_server.pid api_server.log; \
		echo "✅ All dependency data removed"; \
	else \
		echo "Cancelled"; \
	fi

.PHONY: dev-deps-status
dev-deps-status:
	@echo "Dependency Services Status:"
	@echo "==========================="
	@cd deployment/all-in-one && docker compose ps matrixone redis

.PHONY: dev-deps-logs
dev-deps-logs:
	@cd deployment/all-in-one && docker compose logs -f matrixone redis

.PHONY: dev-deps-logs-db
dev-deps-logs-db:
	@cd deployment/all-in-one && docker compose logs -f matrixone

.PHONY: dev-deps-logs-redis
dev-deps-logs-redis:
	@cd deployment/all-in-one && docker compose logs -f redis

.PHONY: dev-logs-clean
dev-logs-clean:
	@echo "⚠️  Clearing Docker logs..."
	@docker logs --tail 0 all-in-one-matrixone-1 2>/dev/null || true
	@docker logs --tail 0 all-in-one-redis-1 2>/dev/null || true
	@echo "✅ Docker logs cleared"

.PHONY: dev-deps-wait
dev-deps-wait:
	@echo "Waiting for dependency services (max 20s)..."
	@for i in 1 2 3 4 5 6 7 8 9 10; do \
		if [ "$$(docker inspect --format='{{.State.Running}}' all-in-one-matrixone-1 2>/dev/null)" = "true" ]; then \
			echo "✅ Dependency services ready"; \
			exit 0; \
		fi; \
		echo "  Waiting... ($$i/10)"; \
		sleep 2; \
	done; \
	echo "❌ Dependency services not ready after 20s"; \
	echo "   Tip: Services may still be starting. Check with 'make dev-deps-status'"; \
	exit 1

.PHONY: dev-db-connect
dev-db-connect:
	@mysql -h127.0.0.1 -P6001 -uroot -p111

# ============================================================================
# Development - API Server (Source Code Mode)
# ============================================================================

.PHONY: dev-api-start
dev-api-start:
	@./scripts/dev/start-api.sh

.PHONY: dev-api-stop
dev-api-stop:
	@echo "Stopping API server..."
	@./scripts/dev/stop-api.sh

.PHONY: dev-api-restart
dev-api-restart: dev-api-stop
	@sleep 1
	@$(MAKE) dev-api-start

.PHONY: dev-api-logs
dev-api-logs:
	@if [ -f api_server.log ]; then \
		tail -f api_server.log; \
	else \
		echo "❌ api_server.log not found. Is API server running?"; \
	fi

.PHONY: dev-api-status
dev-api-status:
	@echo "API Server Status:"
	@echo "=================="
	@if [ -f api_server.pid ] && kill -0 $$(cat api_server.pid) 2>/dev/null; then \
		echo "  ✅ Running (PID: $$(cat api_server.pid))"; \
		if command -v jq >/dev/null 2>&1; then \
			NO_PROXY=localhost curl -s http://localhost:8000/health 2>/dev/null | jq . || echo "  ⚠️  Health check failed"; \
		else \
			NO_PROXY=localhost curl -s http://localhost:8000/health 2>/dev/null || echo "  ⚠️  Health check failed"; \
		fi; \
	else \
		echo "  ❌ Not running"; \
	fi

# ============================================================================
# Development - API Server (Docker Mode)
# ============================================================================

.PHONY: dev-api-docker-build
dev-api-docker-build:
	@echo "Building API server image..."
	@docker build -t mo-agent-engine:latest .
	@echo "✅ Image built"

.PHONY: dev-api-docker-up
dev-api-docker-up:
	@echo "Starting API server (Docker mode)..."
	@cd deployment/all-in-one && docker compose --profile app up -d --build api
	@echo "✅ API server container started"

.PHONY: dev-api-docker-down
dev-api-docker-down:
	@echo "Stopping API server containers..."
	@cd deployment/all-in-one && docker compose --profile app down
	@echo "✅ API server containers stopped"

.PHONY: dev-api-docker-logs
dev-api-docker-logs:
	@cd deployment/all-in-one && docker compose logs -f api

.PHONY: dev-api-docker-scale
dev-api-docker-scale:
	@if [ -z "$(REPLICAS)" ]; then \
		echo "❌ Usage: make dev-api-docker-scale REPLICAS=N"; \
		exit 1; \
	fi
	@echo "Scaling API server to $(REPLICAS) replicas..."
	@cd deployment/all-in-one && docker compose --profile app up -d --scale api=$(REPLICAS)
	@echo "✅ Scaled to $(REPLICAS) replicas"

# ============================================================================
# Development - Composite Commands (Most Used)
# ============================================================================

.PHONY: dev-up
dev-up: dev-deps-up dev-deps-wait dev-api-start
	@echo ""
	@echo "✅ Development environment started!"
	@echo "   API: http://localhost:8000"
	@echo "   Docs: http://localhost:8000/docs"
	@echo ""
	@echo "⚠️  Note: Dependencies may still be starting. Check status with:"
	@echo "   make dev-status"
	@echo ""
	@echo "Next steps:"
	@echo "  mo-agent register"
	@echo "  mo-agent login"
	@echo "  mo-agent chat"

.PHONY: dev-up-docker
dev-up-docker: dev-deps-up dev-deps-wait dev-api-docker-up
	@sleep 3
	@echo ""
	@echo "✅ Development environment ready (Docker mode)!"
	@echo "   API: http://localhost:8000"
	@echo "   Docs: http://localhost:8000/docs"

.PHONY: dev-down
dev-down: dev-api-stop dev-deps-down
	@echo "✅ All services stopped"

.PHONY: dev-restart
dev-restart: dev-down
	@sleep 1
	@$(MAKE) dev-up

.PHONY: dev-status
dev-status:
	@echo ""
	@$(MAKE) dev-deps-status
	@echo ""
	@$(MAKE) dev-api-status

dev-clean: dev-api-stop dev-deps-clean dev-logs-clean
	@echo "✅ Development environment cleaned"

.PHONY: dev-reset
dev-reset: dev-clean
	@$(MAKE) dev-init
	@echo "✅ Development environment reset"

# Configuration validation
.PHONY: dev-config-check dev-config-check-strict
dev-config-check: ## Check development configuration
	@echo "Checking development configuration..."
	@python core/config_validation.py

dev-config-check-strict: ## Check development configuration (strict mode)
	@echo "Checking development configuration (strict mode)..."
	@python core/config_validation.py --strict

# Aliases: README uses dev-start/dev-stop, Makefile defines dev-up/dev-down
.PHONY: dev-start dev-stop dev-start-docker
dev-start: dev-up
dev-stop: dev-down
dev-start-docker: dev-up-docker

# Test aliases: README uses dev-test-*, Makefile defines test-*
.PHONY: dev-test dev-test-keep dev-test-unit dev-test-integration
dev-test: test
dev-test-keep: test
dev-test-unit: test-unit
dev-test-integration: test-integration

# ============================================================================
# Development - Testing
# ============================================================================

.PHONY: test
test:
	@echo "Running all tests..."
	@if ! docker ps | grep -q matrixone; then \
		echo "❌ Error: MatrixOne is not running. Start services with 'make dev-start'"; \
		exit 1; \
	fi
	@python -m pytest tests/ -n auto --dist loadscope -v -m "not slow and not benchmark and not local_embedding"
	@if [ -f memoria/tests/test_e2e.py ]; then \
		python -m pytest memoria/tests/test_e2e.py -n auto --dist loadscope -v; \
	else \
		echo "Skipping Memoria E2E tests: memoria/tests/test_e2e.py not found"; \
	fi

.PHONY: test-cloud
test-cloud:
	@echo "Running Memoria Docker regression tests..."
	@echo "Requires: make cloud-start"
	@if [ -f memoria/tests/test_docker.py ]; then \
		python -m pytest memoria/tests/test_docker.py -v; \
	else \
		echo "Skipping Memoria Docker regression tests: memoria/tests/test_docker.py not found"; \
	fi

.PHONY: cloud-start
cloud-start:
	@echo "Starting Memoria..."
	@docker compose -f memoria/docker-compose.yml up -d
	@echo "API: http://localhost:8100  Swagger: http://localhost:8100/docs"

.PHONY: cloud-stop
cloud-stop:
	@docker compose -f memoria/docker-compose.yml down

.PHONY: cloud-logs
cloud-logs:
	@docker compose -f memoria/docker-compose.yml logs -f api

.PHONY: cloud-status
cloud-status:
	@docker compose -f memoria/docker-compose.yml ps

.PHONY: cloud-clean
cloud-clean:
	@echo "Stopping and removing Memoria (including data)..."
	@docker compose -f memoria/docker-compose.yml down
	@rm -rf memoria/data/
	@echo "Done."

.PHONY: ci-test
ci-test:
	@echo "Running CI test suite (excludes local_embedding, slow, benchmark)..."
	@if [ -d memoria/tests ]; then \
		set -a && [ -f memoria/.env ] && . memoria/.env; set +a; \
		python -m pytest tests/ memoria/tests/ \
			-n auto --dist loadgroup -v --tb=short \
			-m "not slow and not benchmark and not local_embedding"; \
	else \
		python -m pytest tests/ \
			-n auto --dist loadgroup -v --tb=short \
			-m "not slow and not benchmark and not local_embedding"; \
		echo "Skipping Memoria test suite: memoria/tests/ not found"; \
	fi

.PHONY: test-unit
test-unit:
	@echo "Running unit tests..."
	@python -m pytest tests/unit/ -n auto --dist loadscope -v

.PHONY: test-integration
test-integration:
	@echo "Running integration tests..."
	@if ! docker ps | grep -q matrixone; then \
		echo "❌ Error: MatrixOne is not running. Start services with 'make dev-start'"; \
		exit 1; \
	fi
	@python -m pytest tests/integration/ -n auto --dist loadscope -v

# ============================================================================
# Legacy Aliases (Removed - Use dev-* commands instead)
# ============================================================================

.PHONY: db-init-agent
db-init-agent:
	@echo "❌ Deprecated: Use 'make dev-init' instead"
	@exit 1

.PHONY: db-connect
db-connect:
	@echo "❌ Deprecated: Use 'make dev-db-connect' instead"
	@exit 1

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
# E2E Verification
# ============================================================================

.PHONY: verify verify-talk
verify:
	@echo "Running E2E verification (talk-based)..."
	@set -a && . ./.env && set +a && http_proxy= https_proxy= HTTP_PROXY= HTTPS_PROXY= HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 MEMORIA_BASE_URL=http://localhost:8100 MEMORIA_MASTER_KEY=test-master-key-for-docker-compose python scripts/e2e/verify_talk.py $(if $(VERBOSE),-v) $(if $(CASE),--case $(CASE)) $(if $(MODEL),--model $(MODEL))

verify-talk:
	@echo "Running talk verification (requires API server + LLM)..."
	@set -a && . ./.env && set +a && http_proxy= https_proxy= HTTP_PROXY= HTTPS_PROXY= HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 MEMORIA_BASE_URL=http://localhost:8100 MEMORIA_MASTER_KEY=test-master-key-for-docker-compose python scripts/e2e/verify_talk.py $(if $(VERBOSE),-v) $(if $(CASE),--case $(CASE)) $(if $(MODEL),--model $(MODEL))


# ============================================================================
# Testing
# ============================================================================



.PHONY: test-cleanup
test-cleanup:
	@echo "Cleaning up test databases..."
	@python scripts/dev/cleanup_test_dbs.py

.PHONY: test-api
test-api:
	@echo "Running API integration tests..."
	@python -m pytest tests/integration/api/ -v

.PHONY: test-e2e
test-e2e:
	@echo "Running end-to-end tests..."
	@set -a && . ./.env && set +a && python -m pytest tests/e2e/ -v

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
ci: dev-deps-up dev-deps-wait check test dev-deps-down
	@echo "✅ All CI checks passed!"

# ── Memoria Lite publish ─────────────────────────────────────────────

BUMP ?= patch
.PHONY: bump-memoria-version
bump-memoria-version:
	@python scripts/bump_memoria_version.py $(BUMP)

MEMORIA_DIST = dist/memoria

.PHONY: build-memoria
build-memoria:
	@echo "Building memoria-lite..."
	@rm -rf $(MEMORIA_DIST)
	@mkdir -p $(MEMORIA_DIST)
	@cp pyproject.toml pyproject.toml.bak
	@cp pyproject.memoria.toml pyproject.toml
	@python -m build --wheel --outdir $(MEMORIA_DIST)
	@mv pyproject.toml.bak pyproject.toml
	@echo "✅ Built: $$(ls $(MEMORIA_DIST)/*.whl)"

.PHONY: publish-memoria
publish-memoria: build-memoria
	@echo "Publishing memoria-lite to PyPI..."
	@pip install --quiet twine 2>/dev/null || true
	@twine upload $(MEMORIA_DIST)/*
	@echo "✅ Published memoria-lite to PyPI"

.PHONY: publish-memoria-test
publish-memoria-test: build-memoria
	@echo "Publishing memoria-lite to TestPyPI..."
	@pip install --quiet twine 2>/dev/null || true
	@twine upload --repository testpypi $(MEMORIA_DIST)/*
	@echo "✅ Published memoria-lite to TestPyPI"
