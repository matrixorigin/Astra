# astra-engine Makefile

.PHONY: help
help:
	@echo "astra-engine Development Commands"
	@echo "=================================="
	@echo ""
	@echo "Quick Start:"
	@echo "  make dev-start          - Start all (deps + API server)"
	@echo "  make dev-stop           - Stop all services"
	@echo "  make dev-status         - Show all service status"
	@echo "  make dev-init           - Initialize development environment"
	@echo ""
	@echo "Dependencies (MatrixOne + Redis):"
	@echo "  make dev-deps-up        - Start dependency services"
	@echo "  make dev-deps-down      - Stop dependency services"
	@echo "  make dev-deps-clean     - Stop and remove all data (destructive!)"
	@echo "  make dev-deps-status    - Show dependency status"
	@echo "  make dev-deps-logs      - Show dependency logs"
	@echo "  make dev-db-connect     - Connect to MatrixOne CLI"
	@echo ""
	@echo "API Server:"
	@echo "  make dev-api-start      - Start API server"
	@echo "  make dev-api-stop       - Stop API server"
	@echo "  make dev-api-restart    - Restart API server"
	@echo "  make dev-api-logs       - Show API server logs"
	@echo "  make dev-api-status     - Show API server status"
	@echo ""
	@echo "Testing:"
	@echo "  make test               - test-offline + test-live-db (MatrixOne + Redis required for live portion)"
	@echo "  make test-offline       - Workspace tests + astra-runtime bridge-e2e-hooks only (no #[ignore] live-DB suites)"
	@echo "  make test-live-db       - Ignored Matrix HTTP E2E + astra-services multi_agent_integration (exports opt-in env vars)"
	@echo "  make test-contract      - Run contract tests (http/admin/config)"
	@echo ""
	@echo "Code Quality:"
	@echo "  make check              - Run all static checks (lint + format + type)"
	@echo "  make ci                 - Run CI checks (check + test)"
	@echo "  make lint               - Run clippy (warnings are errors)"
	@echo "  make format             - Format code"
	@echo "  make format-check       - Check formatting"
	@echo ""
	@echo "Build:"
	@echo "  make build              - Build entire Rust workspace (release)"
	@echo "  make build-release      - Build entire Rust workspace (release)"
	@echo "  make build-server       - Build astra-server (release)"
	@echo "  make build-server-release - Build astra-server (release)"
	@echo "  make build-cli          - Build astra + astra-admin (release)"
	@echo "  make build-cli-release  - Build astra + astra-admin (release)"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean              - Remove Rust build artifacts (target/)"
	@echo ""
	@echo "Memoria (Memory Service):"
	@echo "  make memoria-start      - Start Memoria service"
	@echo "  make memoria-stop       - Stop Memoria service"
	@echo "  make memoria-logs       - Show Memoria logs"
	@echo "  make memoria-status     - Show Memoria status"
	@echo "  make memoria-clean      - Stop and remove Memoria data"
	@echo ""
	@echo "Docker API (alternative to source mode):"
	@echo "  make dev-start-docker   - Start deps + API in Docker"
	@echo "  make dev-api-docker-up  - Start API server in Docker"
	@echo "  make dev-api-docker-down - Stop API server Docker container"

# ============================================================================
# Variables
# ============================================================================

CARGO_MANIFEST := rust/Cargo.toml
CARGO := cargo
CARGO_MANIFEST_FLAG := --manifest-path $(CARGO_MANIFEST)
API_SHELL_PKG := -p astra-runtime
RUST_TARGET_DIR := rust/target
RUST_DEBUG_BIN_DIR := $(RUST_TARGET_DIR)/debug
RUST_RELEASE_BIN_DIR := $(RUST_TARGET_DIR)/release
API_SERVER_BIN := astra-server
CLI_BINS := astra astra-admin

# ============================================================================
# Environment Setup
# ============================================================================

.PHONY: dev-init
dev-init: setup install-dev-deps
	@echo "Initializing development environment..."
	@bash scripts/dev/init.sh
	@echo ""
	@echo "✅ Development environment initialized!"
	@echo "Next: make dev-start"

.PHONY: setup
setup:
	@echo "Setting up astra-engine development environment..."
	@if [ ! -f .env ]; then \
		cp .env.example .env; \
		echo "✅ Created .env file (please review and customize)"; \
	else \
		echo "⚠️  .env already exists, skipping"; \
	fi

.PHONY: install-dev-deps
install-dev-deps:
	@echo "Installing Rust workspace dependencies..."
	@cargo fetch --manifest-path rust/Cargo.toml
	@echo "✅ Rust dependencies ready"

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
	@echo "2. Rust API binary:"
	@cargo build -q --manifest-path rust/Cargo.toml -p astra-runtime --release --bin astra-server && echo "   ✅ Rust binary build OK"

# ============================================================================
# Dependencies (MatrixOne + Redis)
# ============================================================================

.PHONY: dev-deps-up
dev-deps-up:
	@echo "Starting dependency services (MatrixOne + Redis)..."
	@if [ -d deployment/all-in-one/data ] && [ "$$(stat -c '%u' deployment/all-in-one/data 2>/dev/null || stat -f '%u' deployment/all-in-one/data 2>/dev/null)" != "$$(id -u)" ]; then \
		echo "❌ Error: Data directory owned by root"; \
		echo "   Run: make dev-clean (to delete data)"; \
		echo "   Or:  sudo chown -R $$(id -u):$$(id -g) deployment/all-in-one/data"; \
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
		(cd deployment/all-in-one && docker compose down -v); \
		if [ -d deployment/all-in-one/data ]; then \
			if [ "$$(stat -c '%u' deployment/all-in-one/data 2>/dev/null || stat -f '%u' deployment/all-in-one/data 2>/dev/null)" != "$$(id -u)" ]; then \
				sudo rm -rf deployment/all-in-one/data; \
			else \
				rm -rf deployment/all-in-one/data; \
			fi; \
		fi; \
		rm -f api_server.pid api_server.log; \
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
	echo "   Tip: Check with 'make dev-deps-status'"; \
	exit 1

.PHONY: dev-db-connect
dev-db-connect:
	@mysql -h127.0.0.1 -P6001 -uroot -p111

# ============================================================================
# API Server (Source Code Mode)
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
# API Server (Docker Mode)
# ============================================================================

.PHONY: dev-api-docker-build
dev-api-docker-build:
	@echo "Building API server image..."
	@docker build -t astra-engine:latest .
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
# Composite Commands
# ============================================================================

.PHONY: dev-start
dev-start: dev-deps-up dev-deps-wait dev-api-start
	@echo ""
	@echo "✅ Development environment started!"
	@echo "   API: http://localhost:8000"
	@echo ""
	@echo "Next steps:"
	@echo "  astra register"
	@echo "  astra login"
	@echo "  astra chat"

.PHONY: dev-start-docker
dev-start-docker: dev-deps-up dev-deps-wait dev-api-docker-up
	@sleep 3
	@echo ""
	@echo "✅ Development environment ready (Docker mode)!"
	@echo "   API: http://localhost:8000"

.PHONY: dev-stop
dev-stop: dev-api-stop dev-deps-down
	@echo "✅ All services stopped"

.PHONY: dev-restart
dev-restart: dev-stop
	@sleep 1
	@$(MAKE) dev-start

.PHONY: dev-status
dev-status:
	@echo ""
	@$(MAKE) dev-deps-status
	@echo ""
	@$(MAKE) dev-api-status

.PHONY: dev-clean
dev-clean: dev-api-stop dev-deps-clean
	@echo "✅ Development environment cleaned"

.PHONY: dev-reset
dev-reset: dev-clean
	@$(MAKE) dev-init
	@echo "✅ Development environment reset"

.PHONY: dev-setup-demo
dev-setup-demo:
	@bash scripts/setup/demo-init.sh

.PHONY: dev-seed
dev-seed:
	@echo "⚠️  This will reset the database and reseed admin + models."
	@printf "Are you sure? [y/N] "; read REPLY; \
	[ "$$REPLY" = "y" ] || [ "$$REPLY" = "Y" ] || { echo "Cancelled"; exit 1; }
	@mysql -h127.0.0.1 -P6001 -uroot -p111 \
		-e "DROP DATABASE IF EXISTS astra_runtime; CREATE DATABASE astra_runtime;" 2>/dev/null || \
	mysql -h127.0.0.1 -P6001 -uroot -p111 --skip-ssl \
		-e "DROP DATABASE IF EXISTS astra_runtime; CREATE DATABASE astra_runtime;" 2>/dev/null || \
	mysql -h127.0.0.1 -P6001 -uroot -p111 --skip_ssl \
		-e "DROP DATABASE IF EXISTS astra_runtime; CREATE DATABASE astra_runtime;"
	@$(MAKE) dev-api-restart build-cli-release
	@sleep 2
	@echo "Registering admin (admin@mo.com)..."
	@NO_PROXY=localhost ./rust/target/release/astra-admin register \
		--username admin --password 11111111 --email admin@mo.com
	@echo "Logging in as admin..."
	@NO_PROXY=localhost ./rust/target/release/astra-admin login \
		--username admin --password 11111111
	@echo "Loading models from .models.yaml..."
	@NO_PROXY=localhost ./rust/target/release/astra-admin model load .models.yaml
	@echo ""
	@echo "✅ Seed complete — admin@mo.com / 11111111"

# ============================================================================
# Build
# ============================================================================

.PHONY: build
build: build-release

.PHONY: build-release
build-release:
	@echo "Building Rust workspace (release)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) --release
	@echo "✅ Release artifacts: $(RUST_RELEASE_BIN_DIR)/"

.PHONY: build-cli
build-cli: build-cli-release

.PHONY: build-cli-release
build-cli-release:
	@echo "Building astra + astra-admin (release)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-cli -p astra-admin-cli --release
	@echo "Binaries:"
	@for bin in $(CLI_BINS); do echo "  $(RUST_RELEASE_BIN_DIR)/$$bin"; done

.PHONY: build-server
build-server: build-server-release

.PHONY: build-server-release
build-server-release:
	@echo "Building astra-server (release)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --release --bin $(API_SERVER_BIN)
	@echo "Binary: $(RUST_RELEASE_BIN_DIR)/$(API_SERVER_BIN)"

# ============================================================================
# Cleanup
# ============================================================================

.PHONY: clean
clean:
	@echo "Removing Rust build artifacts..."
	@$(CARGO) clean $(CARGO_MANIFEST_FLAG)
	@echo "✅ Build artifacts removed"

.PHONY: clean-incremental
clean-incremental:
	@echo "Cleaning incremental compilation cache..."
	@rm -rf $(RUST_TARGET_DIR)/debug/incremental
	@echo "✅ Incremental cache removed"

# ============================================================================
# Testing
# ============================================================================

.PHONY: test test-offline test-live-db
test: test-offline test-live-db

.PHONY: test-offline
test-offline: test-workspace test-runtime-bridge-hooks

.PHONY: test-workspace
test-workspace:
	@echo "Running Rust workspace tests (all members, default features)..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG)

# Compiles chat/turn bridge hook paths and runs integration binaries that require
# `required-features = ["bridge-e2e-hooks"]` (e.g. chat_turn_bridge_ledger_inject_e2e).
.PHONY: test-runtime-bridge-hooks
test-runtime-bridge-hooks:
	@echo "Running astra-runtime tests with feature bridge-e2e-hooks..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --features bridge-e2e-hooks

# Ignored tests: opt-in via env vars (see `make test-live-db`). Enable with:
#   ASTRA_SYSTEM_MATRIX_E2E=1   -> system_matrix_http_e2e (--ignored)
#   ASTRA_MULTI_AGENT_IT=1      -> astra-services multi_agent_integration (--ignored)
# Optional serial Matrix E2E: ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS=1 -> --test-threads=1
.PHONY: test-ignored-integration
test-ignored-integration:
	@if [ "$${ASTRA_SYSTEM_MATRIX_E2E:-}" != "1" ] && [ "$${ASTRA_MULTI_AGENT_IT:-}" != "1" ]; then \
		echo "Note: no live-DB ignored suites selected (neither ASTRA_SYSTEM_MATRIX_E2E=1 nor ASTRA_MULTI_AGENT_IT=1). Use \`make test-live-db\` or set those variables."; \
	fi
	@if [ "$${ASTRA_SYSTEM_MATRIX_E2E:-}" = "1" ]; then \
		EXTRA_THREADS=""; \
		if [ "$${ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS:-}" = "1" ]; then \
			EXTRA_THREADS="--test-threads=1"; \
			echo "system_matrix_http_e2e: serial mode (ASTRA_SYSTEM_MATRIX_E2E_TEST_THREADS=1)"; \
		else \
			echo "Running system_matrix_http_e2e (ignored; parallel default; live DB + AppSettings::from_env)..."; \
		fi; \
		$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --features bridge-e2e-hooks \
			--test system_matrix_http_e2e -- --ignored $$EXTRA_THREADS --nocapture; \
	fi
	@if [ "$${ASTRA_MULTI_AGENT_IT:-}" = "1" ]; then \
		echo "Running multi_agent_integration (ignored; live MatrixOne)..."; \
		$(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-services --test multi_agent_integration -- --ignored; \
	fi

# Live MatrixOne + Redis: runs opt-in #[ignore] integration binaries (see test-ignored-integration).
.PHONY: test-live-db
test-live-db:
	@ASTRA_SYSTEM_MATRIX_E2E=1 ASTRA_MULTI_AGENT_IT=1 $(MAKE) test-ignored-integration

.PHONY: test-contract
test-contract:
	@echo "Running core HTTP contract binaries (http/admin) + astra-core settings JSON contract..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) \
		--test http_contract --test admin_contract
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-core --lib settings_contract_tests

# ============================================================================
# Code Quality
# ============================================================================

.PHONY: check
check: lint format-check type-check
	@echo "✅ All static checks passed!"

.PHONY: ci
ci: check test
	@echo "✅ All CI checks passed!"

.PHONY: lint
lint:
	@echo "Running clippy..."
	@$(CARGO) clippy $(CARGO_MANIFEST_FLAG) --all-targets -- -D warnings

.PHONY: lint-fix
lint-fix:
	@$(CARGO) fmt $(CARGO_MANIFEST_FLAG) --all

.PHONY: format
format:
	@$(CARGO) fmt $(CARGO_MANIFEST_FLAG) --all

.PHONY: format-check
format-check:
	@echo "Checking formatting..."
	@$(CARGO) fmt $(CARGO_MANIFEST_FLAG) --all -- --check

.PHONY: type-check
type-check:
	@echo "Running compile checks..."
	@$(CARGO) check $(CARGO_MANIFEST_FLAG) --all-targets

# ============================================================================
# Memoria (Memory Service)
# ============================================================================

.PHONY: memoria-start
memoria-start:
	@echo "Starting Memoria..."
	@docker compose -f memoria/docker-compose.yml up -d
	@echo "API: http://localhost:8100  Swagger: http://localhost:8100/docs"

.PHONY: memoria-stop
memoria-stop:
	@docker compose -f memoria/docker-compose.yml down

.PHONY: memoria-logs
memoria-logs:
	@docker compose -f memoria/docker-compose.yml logs -f api

.PHONY: memoria-status
memoria-status:
	@docker compose -f memoria/docker-compose.yml ps

.PHONY: memoria-clean
memoria-clean:
	@echo "Stopping and removing Memoria (including data)..."
	@docker compose -f memoria/docker-compose.yml down
	@rm -rf memoria/data/
	@echo "Done."

# ============================================================================
# Database
# ============================================================================

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
		echo "✅ Database reset complete"; \
	else \
		echo "Cancelled"; \
	fi
