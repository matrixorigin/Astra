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
	@echo "Dependencies (MatrixOne + Memoria):"
	@echo "  make dev-deps-up        - Start MatrixOne + Memoria"
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
	@echo "  make test               - test-offline + test-online (Rust DB online; optional SDK remote E2E if ASTRA_SDK_ONLINE_E2E=1)"
	@echo "  make test-offline       - Rust workspace + bridge-e2e-hooks + @astra/sdk (1s per case via profile=strict; override: NEXTEST_OFFLINE_PROFILE=<profile>)"
	@echo "  make test-online        - Rust #[ignore] + Matrix E2E (2s per case via profile=strict-online; CI uses strict-online-ci=8s; set ASTRA_SDK_ONLINE_E2E=1 + API for make test-sdk-online)"
	@echo "  make test-live-llm      - Live LLM suite (real provider APIs from .models.yaml; one model per provider)"
	@echo "  make test-contract      - Run contract tests (http/admin/config)"
	@echo "  (also: test-sdk-offline, test-sdk-online — @astra/sdk; offline in test-offline; remote E2E opt-in on test-online)"
	@echo ""
	@echo "Code Quality:"
	@echo "  make check              - Run all static checks (lint + format + type)"
	@echo "  make ci                 - Run CI checks (check + test)"
	@echo "  make lint               - Run clippy (warnings are errors)"
	@echo "  make audit              - Run cargo-audit on rust/ (needs: cargo install cargo-audit)"
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

# Per-test-case hard budget. Any case running longer than the budget is
# killed and counted as FAIL. Nextest has no CLI override for slow-timeout
# (`--config` is Cargo-only, not nextest), so budgets live as named profiles
# in `rust/.config/nextest.toml`:
#   offline:      profile `strict`            → 1s
#   online:       profile `strict-online`     → 2s
#   online (CI):  profile `strict-online-ci`  → 8s
# To switch budgets, override the profile name:
#   make test-online NEXTEST_ONLINE_PROFILE=strict-online-ci
NEXTEST_OFFLINE_PROFILE ?= strict
NEXTEST_ONLINE_PROFILE  ?= strict-online

NEXTEST_OFFLINE_FLAGS := --profile $(NEXTEST_OFFLINE_PROFILE)
NEXTEST_ONLINE_FLAGS  := --profile $(NEXTEST_ONLINE_PROFILE)

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
# Dependencies (MatrixOne + Memoria)
# ============================================================================

DEPS_COMPOSE := cd deployment/all-in-one && env UID=$$(id -u) GID=$$(id -g) docker compose -f docker-compose.deps.yml --env-file ../../.env

.PHONY: dev-deps-up
dev-deps-up:
	@echo "Starting dependency services (MatrixOne + Memoria)..."
	@if [ -d deployment/all-in-one/data ] && [ "$$(stat -c '%u' deployment/all-in-one/data 2>/dev/null || stat -f '%u' deployment/all-in-one/data 2>/dev/null)" != "$$(id -u)" ]; then \
		echo "❌ Error: Data directory owned by root"; \
		echo "   Run: make dev-deps-clean"; \
		echo "   Or:  sudo chown -R $$(id -u):$$(id -g) deployment/all-in-one/data"; \
		exit 1; \
	fi
	@mkdir -p deployment/all-in-one/data/matrixone deployment/all-in-one/data/matrixone/logs deployment/all-in-one/data/logs/memoria
	@$(DEPS_COMPOSE) up -d
	@echo "✅ Dependency services started (MatrixOne :6001, Memoria :8100)"

.PHONY: dev-deps-down
dev-deps-down:
	@echo "Stopping dependency services..."
	@$(DEPS_COMPOSE) down
	@echo "✅ Dependency services stopped"

.PHONY: dev-deps-clean
dev-deps-clean:
	@echo "⚠️  WARNING: This will delete all dependency data!"
	@printf "Are you sure? [y/N] " && read REPLY && \
	if [ "$$REPLY" = "y" ] || [ "$$REPLY" = "Y" ]; then \
		($(DEPS_COMPOSE) down -v); \
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
	@$(DEPS_COMPOSE) ps

.PHONY: dev-deps-logs
dev-deps-logs:
	@$(DEPS_COMPOSE) logs -f

.PHONY: dev-deps-logs-once
dev-deps-logs-once:
	@$(DEPS_COMPOSE) logs --no-color

.PHONY: dev-deps-wait
dev-deps-wait:
	@echo "Waiting for MatrixOne..."
	@for i in $$(seq 1 90); do \
		if curl --noproxy '*' -sf "http://127.0.0.1:$${MATRIXONE_DEBUG_HTTP_PORT:-6060}/debug/vars" >/dev/null 2>&1; then \
			echo "✅ MatrixOne is healthy"; \
			break; \
		fi; \
		if [ "$$i" -eq 90 ]; then \
			echo "❌ MatrixOne not ready after 180s"; \
			echo "   Tip: Check with 'make dev-deps-status' or 'make dev-deps-logs-once'"; \
			exit 1; \
		fi; \
		echo "  Waiting for MatrixOne... ($$i/90)"; \
		sleep 2; \
	done
	@echo "Waiting for Memoria..."
	@for i in $$(seq 1 60); do \
		if curl --noproxy '*' -sf "http://127.0.0.1:$${MEMORIA_PORT:-8100}/health" >/dev/null 2>&1; then \
			echo "✅ Memoria is healthy"; \
			echo "✅ Dependency services ready"; \
			exit 0; \
		fi; \
		if [ "$$i" -eq 60 ]; then \
			echo "❌ Memoria not ready after 120s"; \
			echo "   Tip: Check with 'make dev-deps-status' or 'make dev-deps-logs-once'"; \
			exit 1; \
		fi; \
		echo "  Waiting for Memoria... ($$i/60)"; \
		sleep 2; \
	done

.PHONY: dev-db-connect
dev-db-connect:
	@set -a; [ -f .env ] && . ./.env; set +a; \
	mysql -h$${MATRIXONE_HOST:-127.0.0.1} -P$${MATRIXONE_PORT:-6001} -u$${MATRIXONE_USER:-root} -p$${MATRIXONE_PASSWORD:-111}

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
	@set -a; [ -f .env ] && . ./.env; set +a; \
	DB_HOST=$${MATRIXONE_HOST:-127.0.0.1}; \
	DB_PORT=$${MATRIXONE_PORT:-6001}; \
	DB_USER=$${MATRIXONE_USER:-root}; \
	DB_PASS=$${MATRIXONE_PASSWORD:-111}; \
	DB_NAME=$${ASTRA_DATABASE:-astra_runtime}; \
	mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS \
		-e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
	mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS --skip-ssl \
		-e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
	mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS --skip_ssl \
		-e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;"
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
build-release: sweep
	@echo "Building Rust workspace (release)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) --release
	@touch $(RUST_TARGET_DIR)/.sweep-stamp
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

.PHONY: clean-debug
clean-debug:
	@echo "Removing debug build artifacts..."
	@rm -rf $(RUST_TARGET_DIR)/debug
	@echo "✅ Debug artifacts removed"

.PHONY: clean-incremental
clean-incremental:
	@echo "Cleaning incremental compilation cache..."
	@rm -rf $(RUST_TARGET_DIR)/debug/incremental
	@echo "✅ Incremental cache removed"

SWEEP_STAMP := $(RUST_TARGET_DIR)/.sweep-stamp

.PHONY: sweep-stamp
sweep-stamp:
	@touch $(SWEEP_STAMP)

.PHONY: sweep
sweep:
	@if [ -f $(SWEEP_STAMP) ]; then \
		echo "Sweeping artifacts inactive since $$(stat -c '%y' $(SWEEP_STAMP) | cut -d. -f1)..."; \
		find $(RUST_TARGET_DIR)/debug/incremental -maxdepth 1 -type d ! -newer $(SWEEP_STAMP) -exec rm -rf {} + 2>/dev/null || true; \
		find $(RUST_TARGET_DIR)/debug/deps -type f ! -newer $(SWEEP_STAMP) -delete 2>/dev/null || true; \
		find $(RUST_TARGET_DIR)/debug/.fingerprint -maxdepth 1 -type d ! -newer $(SWEEP_STAMP) -exec rm -rf {} + 2>/dev/null || true; \
		find $(RUST_TARGET_DIR)/release/incremental -maxdepth 1 -type d ! -newer $(SWEEP_STAMP) -exec rm -rf {} + 2>/dev/null || true; \
	else \
		echo "No stamp found, sweeping artifacts older than 5 days..."; \
		find $(RUST_TARGET_DIR)/debug/incremental -maxdepth 1 -type d -mtime +5 -exec rm -rf {} + 2>/dev/null || true; \
		find $(RUST_TARGET_DIR)/debug/deps -type f -atime +5 -delete 2>/dev/null || true; \
		find $(RUST_TARGET_DIR)/debug/.fingerprint -maxdepth 1 -type d -mtime +5 -exec rm -rf {} + 2>/dev/null || true; \
		find $(RUST_TARGET_DIR)/release/incremental -maxdepth 1 -type d -mtime +5 -exec rm -rf {} + 2>/dev/null || true; \
	fi
	@echo "✅ Inactive artifacts removed"
	@du -sh $(RUST_TARGET_DIR) 2>/dev/null || true

# ============================================================================
# Testing
# ============================================================================

.PHONY: test test-offline test-online test-sdk-offline test-sdk-online
test: test-offline test-online

.PHONY: test-offline
test-offline: test-workspace test-runtime-bridge-hooks test-sdk-offline

.PHONY: test-workspace
test-workspace:
	@echo "Running Rust workspace tests (nextest profile=$(NEXTEST_OFFLINE_PROFILE))..."
	@cargo nextest run $(CARGO_MANIFEST_FLAG) --workspace $(NEXTEST_OFFLINE_FLAGS)
	@echo "Running workspace doctests (cargo test --doc; not covered by nextest)..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) --workspace --doc

# Guard: server allowlist names ⊆ all_tool_schemas(), memory_* allowlist coverage,
# DEFAULT_EXECUTOR_TOOL_NAMES ⊆ SERVER_EXECUTOR_TOOL_NAMES (manifest-path rust/Cargo.toml).
.PHONY: check-server-tool-schemas
check-server-tool-schemas:
	@echo "Running astra-tools server/allowlist/schema guard tests..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-tools schemas::tests::

# Compiles chat/turn bridge hook paths and runs integration binaries that require
# `required-features = ["bridge-e2e-hooks"]` (e.g. chat_turn_bridge_ledger_inject_e2e).
.PHONY: test-runtime-bridge-hooks
test-runtime-bridge-hooks:
	@echo "Running astra-runtime tests with feature bridge-e2e-hooks (nextest profile=$(NEXTEST_OFFLINE_PROFILE))..."
	@cargo nextest run $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) \
		--features bridge-e2e-hooks $(NEXTEST_OFFLINE_FLAGS)

# Ignored tests: opt-in via env vars (see `make test-online`). Enable with:
#   ASTRA_TEST_DB_IT=1   -> all online/Matrix ignored integration tests (--ignored)
#
# Single `cargo nextest run` covers every `#[ignore]` integration binary
# across astra-runtime / astra-services / astra-plan. nextest itself
# tracks TIMEOUT/FAIL per-case, emits per-case status lines during the
# run, and prints the standard `Summary [ … ]` at the end — same UX as
# `make test-offline`. No per-suite tee/log bookkeeping needed.
#
# Optional serial mode: ASTRA_TEST_DB_IT_TEST_THREADS=1 -> -j 1
.PHONY: test-ignored-integration
test-ignored-integration:
	@if [ "$${ASTRA_TEST_DB_IT:-}" != "1" ]; then \
		echo "Note: no online/Matrix ignored suites selected. Use \`make test-online\` or set ASTRA_TEST_DB_IT=1."; \
	fi
	@if [ "$${ASTRA_TEST_DB_IT:-}" = "1" ]; then \
		JOBS_FLAG=""; \
		if [ "$${ASTRA_TEST_DB_IT_TEST_THREADS:-}" = "1" ]; then \
			JOBS_FLAG="-j 1"; \
			echo "Online integration tests: serial mode (ASTRA_TEST_DB_IT_TEST_THREADS=1)"; \
		else \
			echo "Running online integration tests (ignored; live MatrixOne; bridge-e2e-hooks enabled for system_matrix_http_e2e)..."; \
		fi; \
		cargo nextest run $(CARGO_MANIFEST_FLAG) \
			-p astra-runtime -p astra-services -p astra-plan \
			--features astra-runtime/bridge-e2e-hooks \
			--tests --run-ignored only \
			$(NEXTEST_ONLINE_FLAGS) $$JOBS_FLAG; \
	fi

# Online (MatrixOne): opt-in #[ignore] integration binaries (see test-ignored-integration).
# @astra/sdk remote E2E is opt-in (ASTRA_SDK_ONLINE_E2E=1) so CI make test-online has no API on :8000.
.PHONY: test-online
test-online:
	@if [ ! -f .env ]; then \
		echo "No .env found — creating from .env.example..."; \
		cp .env.example .env; \
	fi
	@set -a; [ -f .env ] && . ./.env; set +a; \
	TEST_DB=$${ASTRA_TEST_DATABASE:-astra_runtime_test}; \
	DB_HOST=$${MATRIXONE_HOST:-127.0.0.1}; \
	DB_PORT=$${MATRIXONE_PORT:-6001}; \
	DB_USER=$${MATRIXONE_USER:-root}; \
	DB_PASS=$${MATRIXONE_PASSWORD:-111}; \
	echo "Recreating test database $$TEST_DB ..."; \
	mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS \
		-e "DROP DATABASE IF EXISTS $$TEST_DB; CREATE DATABASE $$TEST_DB;" 2>/dev/null || \
	mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS --skip-ssl \
		-e "DROP DATABASE IF EXISTS $$TEST_DB; CREATE DATABASE $$TEST_DB;" 2>/dev/null || true; \
	echo "Running astra-runtime ignored unit tests (live DB; nextest profile=$(NEXTEST_ONLINE_PROFILE); live-LLM suite gated by ASTRA_LIVE_LLM)..."; \
	FAILED=""; \
	ASTRA_DATABASE=$$TEST_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
		cargo nextest run $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) \
			--run-ignored only $(NEXTEST_ONLINE_FLAGS) \
			|| FAILED="$$FAILED astra-runtime-ignored"; \
	ASTRA_DATABASE=$$TEST_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
		ASTRA_TEST_DB_IT=1 \
		$(MAKE) test-ignored-integration \
		|| FAILED="$$FAILED test-ignored-integration"; \
	if [ -n "$$FAILED" ]; then \
		echo "❌ test-online: failed suites:$$FAILED"; \
		exit 1; \
	fi
	@if [ "$${ASTRA_SDK_ONLINE_E2E:-}" = "1" ]; then \
		$(MAKE) test-sdk-online; \
	else \
		echo "Skipping @astra/sdk remote E2E (set ASTRA_SDK_ONLINE_E2E=1 with API running, or: make test-sdk-online)"; \
	fi
	@echo ""
	@echo "NOTE: live-LLM suite (real provider APIs, reads .models.yaml) auto-skips unless"
	@echo "      ASTRA_LIVE_LLM=1 is set. Run it explicitly with: make test-live-llm"

# Live-LLM suite: hits real provider APIs listed in .models.yaml.
# Picks ONE model per distinct provider at runtime — what's in the yaml gets
# tested, nothing is hard-coded. Bypasses MatrixOne / DB fixtures entirely.
.PHONY: test-live-llm
test-live-llm:
	@echo "Running live-LLM token usage tests (reads .models.yaml; one model per provider)..."
	@ASTRA_LIVE_LLM=1 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) \
		--test live_token_usage_e2e -- --ignored --nocapture

# @astra/sdk — no real HTTP API (Mode A in-process runs via ASTRA_SDK_E2E=1 in test:coverage)
.PHONY: test-sdk-offline
test-sdk-offline:
	@echo "Running @astra/sdk offline (typecheck, Jest with coverage + Mode A E2E, build)..."
	@cd packages/sdk && npm install --no-audit --no-fund --ignore-scripts
	@cd packages/sdk && npm run typecheck
	@cd packages/sdk && ASTRA_SDK_E2E=1 npm run test:coverage
	@cd packages/sdk && npm run build

# @astra/sdk — Jest Mode B (ASTRA_SDK_BASE_URL) + sdk-online-smoke; requires astra-server (e.g. make dev-start)
.PHONY: test-sdk-online
test-sdk-online:
	@echo "Running @astra/sdk online (Jest integration + test:online) — ensure API is up (e.g. make dev-start)..."
	@cd packages/sdk && npm install --no-audit --no-fund --ignore-scripts
	@bash -ec 'set -a; [ -f "$(CURDIR)/.env" ] && . "$(CURDIR)/.env"; set +a; \
		export ASTRA_SDK_E2E=1; \
		export ASTRA_SDK_BASE_URL="$${ASTRA_SDK_BASE_URL:-http://127.0.0.1:$${ASTRA_API_PORT:-8000}}"; \
		cd "$(CURDIR)/packages/sdk" && npm run test:integration:local && npm run test:online'

.PHONY: test-contract
test-contract:
	@echo "Running core HTTP contract binaries (http/admin) + astra-core settings JSON contract..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) \
		--test http_contract --test admin_contract
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-core --lib settings_contract_tests

# ----------------------------------------------------------------------------
# Declarative CLI test harness (astra-test-harness).
#
# Runs the YAML cases at rust/crates/astra-test-harness/cases against
# a fallback model list. Requires a running API server + fresh login.
# Override MODELS for a quick single-model smoke:
#     make test-harness MODELS=qwen-flash
# Override CASES to point at a different suite directory.
# ----------------------------------------------------------------------------
.PHONY: test-harness
test-harness:
	@echo "Running astra-test-harness (unit tests + live suite)..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-test-harness
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-test-harness --release
	@MODELS="$${MODELS:-qwen-flash}"; \
	CASES="$${CASES:-rust/crates/astra-test-harness/cases}"; \
	JUDGER="$${JUDGER:-$$MODELS}"; \
	echo "  cases=$$CASES models=$$MODELS judger=$$JUDGER"; \
	./rust/target/release/astra-test \
		--suite $$CASES \
		--models $$MODELS \
		--judger-model $$JUDGER \
		--no-judger

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
	@$(CARGO) clippy $(CARGO_MANIFEST_FLAG) --release --all-targets -- -D warnings

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

# RustSec dependency audit (same gate as GitHub static-checks workflow).
.PHONY: audit
audit:
	@command -v cargo-audit >/dev/null 2>&1 || { echo "cargo-audit not found; install with: cargo install cargo-audit"; exit 1; }
	@cd rust && cargo audit

.PHONY: type-check
type-check:
	@echo "Running compile checks..."
	@$(CARGO) check $(CARGO_MANIFEST_FLAG) --release --all-targets

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
			DB_NAME=$${ASTRA_DATABASE_PREFIX:-}$${ASTRA_DATABASE:-dev_agent}; \
		else \
			DB_NAME=dev_agent; \
		fi; \
		DB_HOST=$${MATRIXONE_HOST:-127.0.0.1}; \
		DB_PORT=$${MATRIXONE_PORT:-6001}; \
		DB_USER=$${MATRIXONE_USER:-root}; \
		DB_PASS=$${MATRIXONE_PASSWORD:-111}; \
		mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
		mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS --skip-ssl -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;" 2>/dev/null || \
		mysql -h$$DB_HOST -P$$DB_PORT -u$$DB_USER -p$$DB_PASS --skip_ssl -e "DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;"; \
		echo "✅ Database reset complete"; \
	else \
		echo "Cancelled"; \
	fi
