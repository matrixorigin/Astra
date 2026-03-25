# mo-agent-engine Makefile
# Inspired by MatrixOne's development workflow

.PHONY: help
help:
	@echo "mo-agent-engine Development Commands"
	@echo "=================================="
	@echo ""
	@echo "Quick Start:"
	@echo "  make dev-start          - Start all (deps + API server)"
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
	@echo "  make test               - Run all Rust tests"
	@echo "  make test-integration   - Run integration contract tests"
	@echo ""
	@echo "Code Quality:"
	@echo "  make check              - Run all static checks (lint + format + type)"
	@echo "  make ci                 - Run CI checks (check + test, no Docker required)"
	@echo "  make lint               - Run clippy (warnings are errors)"
	@echo "  make lint-fix           - Auto-format code"
	@echo "  make format             - Format code"
	@echo "  make format-check       - Check formatting"
	@echo ""
	@echo "Build:"
	@echo "  make rust-build         - Build the Rust workspace"
	@echo "  make rust-build-release - Build the Rust workspace in release mode"
	@echo "  make cli-build          - Build CLI/API binaries in debug mode"
	@echo "  make cli-build-release  - Build CLI/API binaries in release mode"
	@echo "  make print-bin-paths    - Show debug/release binary paths"
	@echo "  make check-runtime      - Verify runtime environment"
	@echo ""
	@echo "Examples:"
	@echo "  make dev-start                    # Daily development"
	@echo "  make dev-api-restart              # After code changes"
	@echo "  make test                         # Run all tests"
	@echo "  make check                        # Static analysis"

# ============================================================================
# Environment Setup
# ============================================================================

.PHONY: dev-init
dev-init: setup install-dev-deps
	@echo "Initializing development environment..."
	@bash scripts/dev/init.sh
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
	@echo "Installing Rust workspace dependencies..."
	@cargo fetch --manifest-path rust/Cargo.toml
	@echo "✅ Rust dependencies ready"

# Alias for compatibility.
.PHONY: install-check-deps
install-check-deps:
	@$(MAKE) install-dev-deps

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
	@echo "3. Rust API binary:"
	@cargo build -q --manifest-path rust/Cargo.toml -p mo-agent-runtime --bin mo-agent-server && echo "   ✅ Rust binary build OK"

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
	@cargo test --manifest-path rust/Cargo.toml -p mo-agent-runtime --test config_contract

dev-config-check-strict: ## Check development configuration (strict mode)
	@echo "Checking development configuration (strict mode)..."
	@cargo test --manifest-path rust/Cargo.toml -p mo-agent-runtime --test config_contract
	@cargo check --manifest-path rust/Cargo.toml --all-targets

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

CARGO_MANIFEST := rust/Cargo.toml
CARGO := cargo
CARGO_MANIFEST_FLAG := --manifest-path $(CARGO_MANIFEST)
API_SHELL_PKG := -p mo-agent-runtime
API_SHELL_TESTS := $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --tests
RUST_TARGET_DIR := rust/target
RUST_DEBUG_BIN_DIR := $(RUST_TARGET_DIR)/debug
RUST_RELEASE_BIN_DIR := $(RUST_TARGET_DIR)/release
API_SERVER_BIN := mo-agent-server
CLI_BINS := mo-agent mo-admin
ALL_BINS := $(API_SERVER_BIN) $(CLI_BINS)

.PHONY: test test-cloud ci-test test-unit test-integration verify verify-talk
test:
	@echo "Running Rust workspace tests..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG)

test-cloud ci-test test-unit verify verify-talk: test
	@:

test-integration:
	@echo "Running Rust API-shell integration contracts..."
	@$(API_SHELL_TESTS)

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

.PHONY: rust-build
rust-build:
	@echo "Building Rust workspace (debug profile)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG)
	@echo "✅ Debug artifacts: $(RUST_DEBUG_BIN_DIR)/"

.PHONY: rust-build-release
rust-build-release:
	@echo "Building Rust workspace (release profile)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) --release
	@echo "✅ Release artifacts: $(RUST_RELEASE_BIN_DIR)/"

.PHONY: cli-build
cli-build:
	@echo "Building CLI/API binaries (debug profile)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) $(foreach bin,$(ALL_BINS),--bin $(bin))
	@$(MAKE) print-bin-paths

.PHONY: cli-build-release
cli-build-release:
	@echo "Building CLI/API binaries (release profile)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --release $(foreach bin,$(ALL_BINS),--bin $(bin))
	@$(MAKE) print-bin-paths

.PHONY: print-bin-paths
print-bin-paths:
	@echo "Debug binaries:"
	@for bin in $(ALL_BINS); do echo "  $(RUST_DEBUG_BIN_DIR)/$$bin"; done
	@echo "Release binaries:"
	@for bin in $(ALL_BINS); do echo "  $(RUST_RELEASE_BIN_DIR)/$$bin"; done

.PHONY: rust-test
rust-test: test
	@:

.PHONY: migration-contract-test
migration-contract-test:
	@echo "Running Rust HTTP contract tests..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --test http_contract
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --test admin_contract
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --test auth_contract
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --test config_contract

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

# ============================================================================
# Testing
# ============================================================================



.PHONY: test-cleanup
test-cleanup:
	@echo "No extra test DB cleanup needed in Rust-only mode."

.PHONY: test-api
test-api:
	@echo "Running Rust API integration contract tests..."
	@$(API_SHELL_TESTS)

.PHONY: test-e2e
test-e2e:
	@echo "Running Rust end-to-end contract subset..."
	@$(API_SHELL_TESTS)

.PHONY: test-runtime
test-runtime: test
	@echo "✅ Runtime tests complete"

# ============================================================================
# Code Quality
# ============================================================================

# Rust check environment
.PHONY: check-env
check-env:
	@cargo --version >/dev/null 2>&1 || (echo "❌ Error: cargo not found. Install Rust toolchain first." && exit 1)

.PHONY: check
check: check-env lint format-check type-check
	@echo "✅ All static checks passed!"

.PHONY: lint
lint:
	@echo "Running linters..."
	@$(CARGO) clippy $(CARGO_MANIFEST_FLAG) --all-targets -- -D warnings

.PHONY: lint-fix
lint-fix:
	@echo "Rust lint auto-fix via cargo fmt..."
	@$(CARGO) fmt $(CARGO_MANIFEST_FLAG) --all

.PHONY: type-check
type-check:
	@echo "Running Rust compile checks..."
	@$(CARGO) check $(CARGO_MANIFEST_FLAG) --all-targets

.PHONY: format
format:
	@echo "Formatting code..."
	@$(CARGO) fmt $(CARGO_MANIFEST_FLAG) --all

.PHONY: format-check
format-check:
	@echo "Checking code formatting..."
	@$(CARGO) fmt $(CARGO_MANIFEST_FLAG) --all -- --check

.PHONY: pre-commit
.PHONY: ci
ci: check test
	@echo "✅ All CI checks passed!"

# ── Memoria Lite publish ─────────────────────────────────────────────

.PHONY: bump-memoria-version build-memoria publish-memoria publish-memoria-test
bump-memoria-version build-memoria publish-memoria publish-memoria-test:
	@echo "❌ Deprecated in Rust-only mode"
	@exit 1
