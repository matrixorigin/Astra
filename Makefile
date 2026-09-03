# Astra Makefile

.PHONY: help
help:
	@echo "Astra Development Commands"
	@echo "=================================="
	@echo ""
	@echo "Quick Start:"
	@echo "  make dev-start          - Start all (deps + API server + web UI)"
	@echo "  make dev-start-server-only - Start server-only runtime (deps + API + web; no edge provider)"
	@echo "  make dev-start-server-edge - Start server + local astra-edge provider"
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
	@echo "  make dev-api-start      - Start API server (release build)"
	@echo "  make dev-api-start-debug - Start API server (debug build, fast)"
	@echo "  make dev-api-stop       - Stop API server"
	@echo "  make dev-api-restart    - Restart API server (release)"
	@echo "  make dev-api-restart-debug - Restart API server (debug, fast)"
	@echo "  make dev-api-logs       - Show API server logs"
	@echo "  make dev-api-status     - Show API server status"
	@echo ""
	@echo "Web UI:"
	@echo "  make dev-sdk-deps       - Install/build local @astra/sdk for web UI"
	@echo "  make dev-web-deps       - Install web UI dependencies"
	@echo "  make dev-web-start      - Start web UI (default http://localhost:3536; override with ASTRA_WEB_PORT=<port>)"
	@echo "  make dev-web-stop       - Stop web UI"
	@echo "  make dev-web-restart    - Restart web UI"
	@echo "  make dev-web-logs       - Show web UI logs"
	@echo "  make dev-web-status     - Show web UI status"
	@echo ""
	@echo "Edge Provider:"
	@echo "  make dev-edge-start     - Start local astra-edge provider for web/server workspace tools"
	@echo "  make dev-edge-stop      - Stop local astra-edge provider"
	@echo "  make dev-edge-logs      - Show astra-edge logs"
	@echo "  make dev-edge-status    - Show astra-edge status"
	@echo ""
	@echo "Testing:"
	@echo "  make test               - test-offline + test-online (Rust DB online; optional SDK remote E2E if ASTRA_SDK_ONLINE_E2E=1)"
	@echo "  make test-offline       - Rust workspace + e2e-hooks + @astra/sdk (30s per case via profile=strict; override: NEXTEST_OFFLINE_PROFILE=<profile>)"
	@echo "  make validate-capability-matrix - Verify capability system-test references resolve"
	@echo "  make test-online        - Rust #[ignore] + Matrix E2E (30s per case via profile=strict-online; see .config/nextest.toml)"
	@echo "  make test-memoria-online-contract - Real Memoria missing-ID/circuit-recovery contract (explicit)"
	@echo "  make test-runtime-profiles - Server-only + server+edge + managed runtime + CLI-local profile guardrails"
	@echo "  make test-server-only   - Focused Web/runtime tests for server-only access surface"
	@echo "  make test-server-edge   - Focused tests for edge provider protocol and routing"
	@echo "  make test-managed-runtime - Focused tests for sandbox/orchestrator/MCP provider routing"
	@echo "  make test-cli-local     - Focused tests for CLI-local provider routing"
	@echo "  make test-no-sticky-control - Fast no-sticky control-plane tests (approval/ask_user/edge callbacks; no live DB)"
	@echo "  make test-cleanup-pressure - Live MatrixOne cleanup pressure probes (explicit, not part of test-online)"
	@echo "  make test-durable-event-pressure - Live MatrixOne durable event pressure probe (explicit, not part of test-online)"
	@echo "  make test-saas          - SaaS platform E2E (docs/testing/saas-test-plan.md §5; MatrixOne + optional SDK)"
	@echo "  make test-saas-coverage - SaaS E2E + llvm line coverage report (needs: cargo install cargo-llvm-cov)"
	@echo "  make test-live-llm      - Live LLM suite (real provider APIs from .models.yaml; one model per provider)"
	@echo "  make test-contract      - Run contract tests (http/admin/config)"
	@echo "  (also: test-sdk-offline, test-web-offline, test-sdk-online — @astra/sdk + web offline; remote E2E opt-in on test-online)"
	@echo ""
	@echo "Code Quality:"
	@echo "  make check              - Run all static checks (lint + format + type)"
	@echo "  make ci                 - Run CI checks (check + test)"
	@echo "  make lint               - Run clippy (warnings are errors)"
	@echo "  make audit              - Run cargo-audit (needs: cargo install cargo-audit)"
	@echo "  make format             - Format code"
	@echo "  make format-check       - Check formatting"
	@echo ""
	@echo "Build:"
	@echo "  make build              - Build entire Rust workspace (release)"
	@echo "  make build-debug        - Build entire Rust workspace (debug, fast)"
	@echo "  make build-server       - Build astra-server (release)"
	@echo "  make build-server-debug - Build astra-server (debug, fast)"
	@echo "  make build-edge         - Build astra-edge provider (release)"
	@echo "  make build-edge-debug   - Build astra-edge provider (debug, fast)"
	@echo "  make build-cli          - Build astra CLI (release)"
	@echo "  make build-cli-debug    - Build astra CLI (debug, fast)"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean              - Remove ALL Rust build artifacts (target/)"
	@echo "  make clean-stale        - Remove artifacts not accessed in 12h (override: STALE_HOURS=N)"
	@echo "  make clean-debug        - Remove debug/ directory only"
	@echo "  make sweep              - Remove artifacts older than 4h (runs auto before release builds)"
	@echo ""
	@echo "Memoria (Memory Service):"
	@echo "  make memoria-start      - Start Memoria service"
	@echo "  make memoria-stop       - Stop Memoria service"
	@echo "  make memoria-logs       - Show Memoria logs"
	@echo "  make memoria-status     - Show Memoria status"
	@echo ""
	@echo "All-in-One Docker Deployment:"
	@echo "  make stack-env          - Create .env and generate stack secrets"
	@echo "  make stack-up           - Start MatrixOne + Memoria + API"
	@echo "  make stack-up-server-only - Start compose stack without local edge provider"
	@echo "  make stack-up-server-edge - Start compose stack plus local astra-edge provider"
	@echo "  make stack-down         - Stop compose stack"
	@echo "  make stack-clean        - Stop compose stack and remove MatrixOne data"
	@echo "  make stack-status       - Show compose stack status"
	@echo "  make stack-logs         - Follow stack logs (SERVICE=api optional)"
	@echo ""
	@echo "Docker API (alternative to source mode):"
	@echo "  make dev-start-docker   - Start deps + API in Docker"
	@echo "  make dev-api-docker-up  - Start API server in Docker"
	@echo "  make dev-api-docker-down - Stop API server Docker container"
	@echo "  make release-docker     - Build and push Docker image (VERSION=..., CONFIRM=yes)"

# ============================================================================
# Variables
# ============================================================================

CARGO_MANIFEST := Cargo.toml
CARGO := cargo
CARGO_MANIFEST_FLAG := --manifest-path $(CARGO_MANIFEST)
API_SHELL_PKG := -p astra-runtime
RUST_TARGET_DIR := target
RUST_DEBUG_BIN_DIR := $(RUST_TARGET_DIR)/debug
RUST_RELEASE_BIN_DIR := $(RUST_TARGET_DIR)/release
API_SERVER_BIN := astra-server
EDGE_BIN := astra-edge
CLI_BINS := astra
CLI_RELEASE_FLAGS ?= --no-default-features
IMAGE_NAME ?= matrixorigin/astra
DOCKER_BUILD_ARGS ?=
DOCKER_PROXY_BUILD_ARGS := --build-arg http_proxy --build-arg https_proxy --build-arg no_proxy --build-arg HTTP_PROXY --build-arg HTTPS_PROXY --build-arg NO_PROXY
IMAGE_VERSION ?= $(if $(VERSION),$(patsubst v%,%,$(VERSION)),dev)
IMAGE_REVISION ?= $(shell git rev-parse HEAD 2>/dev/null || echo unknown)
IMAGE_SOURCE_DIRTY ?= $(shell if git rev-parse --is-inside-work-tree >/dev/null 2>&1 && test -z "$$(git status --porcelain=v1 --untracked-files=no 2>/dev/null)"; then echo false; else echo true; fi)
IMAGE_BRANCH ?= $(shell git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)
DOCKER_METADATA_BUILD_ARGS := --build-arg IMAGE_VERSION=$(IMAGE_VERSION) --build-arg IMAGE_REVISION=$(IMAGE_REVISION) --build-arg IMAGE_SOURCE_DIRTY=$(IMAGE_SOURCE_DIRTY) --build-arg IMAGE_BRANCH=$(IMAGE_BRANCH)
# Project-wide default for every API server mode. Compose may remap the
# host-facing port, but the container listens on this value.
DEFAULT_API_PORT := 17001
STACK_DIR := deployment/all-in-one
STACK_ENV := $(STACK_DIR)/.env
STACK_COMPOSE := cd $(STACK_DIR) && docker compose --env-file $(abspath $(STACK_ENV))
STACK_SECRET_ENV := ASTRA_JWT_SECRET ASTRA_TOKEN_ENCRYPTION_KEY ASTRA_RUNTIME_ROOT_SECRET MEMORIA_MASTER_KEY
STACK_EMBEDDING_ENV := MEMORIA_EMBEDDING_BASE_URL

# Per-test-case hard budget. Any case running longer than the budget is
# killed and counted as FAIL. Nextest has no CLI override for slow-timeout
# (`--config` is Cargo-only, not nextest), so budgets live as named profiles
# in `.config/nextest.toml`. All profiles currently use 30s (relaxed
# from original 1-2s due to known session_sync_log prune contention — see
# nextest.toml comment for tracking details).
# To switch budgets, override the profile name:
#   make test-online NEXTEST_ONLINE_PROFILE=strict-online-ci
NEXTEST_OFFLINE_PROFILE ?= strict
NEXTEST_ONLINE_PROFILE  ?= strict-online
CLEANUP_PRESSURE_PROFILE ?= smoke
CLEANUP_PRESSURE_DATABASE_BASE ?= astra_runtime_test_cleanup_pressure
CLEANUP_PRESSURE_ARGS ?=
DURABLE_EVENT_PRESSURE_PROFILE ?= smoke
DURABLE_EVENT_PRESSURE_DATABASE ?= astra_runtime_test_durable_event_pressure
DURABLE_EVENT_PRESSURE_ARGS ?=

NEXTEST_OFFLINE_FLAGS := --profile $(NEXTEST_OFFLINE_PROFILE)
NEXTEST_ONLINE_FLAGS  := --profile $(NEXTEST_ONLINE_PROFILE)
# Operational pressure probes have dedicated runners and data-size controls.
# Keep them out of the generic ignored-test lane, whose per-case timeout is a
# correctness budget rather than a load-test budget.
NEXTEST_CLEANUP_PRESSURE_EXCLUSION := not test(/(db_cleanup_expired|db_truncate_gc|prompt_retention)_pressure_probe/)
# Phase-0 production baselines require hermetic binary/model inputs and the
# ASTRA_PHASE0_BASELINE_EXCLUSIVE guard. They are owned by
# scripts/phase0-production-baseline.sh, not the generic ignored-test lane.
NEXTEST_PHASE0_BASELINE_EXCLUSION := not test(/e2e_matrix_phase0_(server_only_production_baseline|external_(production_topologies|edge_server_m1))/)

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
	@echo "Setting up Astra development environment..."
	@if [ ! -f .env ]; then \
		cp .env.example .env; \
		echo "✅ Created .env file (please review and customize)"; \
	else \
		echo "⚠️  .env already exists, skipping"; \
	fi

.PHONY: install-dev-deps
install-dev-deps:
	@echo "Installing Rust workspace dependencies..."
	@cargo fetch --manifest-path Cargo.toml
	@$(MAKE) dev-web-deps
	@echo "✅ Development dependencies ready"

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
	@cargo build -q --manifest-path Cargo.toml -p astra-runtime --release --bin astra-server && echo "   ✅ Rust binary build OK"

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
	@set -a; [ -f .env ] && . ./.env; set +a; \
	if [ -z "$${MEMORIA_MASTER_KEY:-}" ]; then \
		echo "❌ MEMORIA_MASTER_KEY is required for an authenticated readiness check"; \
		exit 2; \
	fi; \
	for i in $$(seq 1 60); do \
		if curl --noproxy '*' -sf \
			-H "Authorization: Bearer $$MEMORIA_MASTER_KEY" \
			"http://127.0.0.1:$${MEMORIA_PORT:-8100}/v1/health/analyze" >/dev/null 2>&1; then \
			echo "✅ Memoria is healthy"; \
			echo "✅ Dependency services ready"; \
			exit 0; \
		fi; \
		if [ "$$i" -eq 60 ]; then \
			echo "❌ Memoria not ready after 120s"; \
			echo "   The process-only /health endpoint is not sufficient; authenticated storage readiness failed."; \
			echo "   Tip: Check with 'make dev-deps-status' or 'make dev-deps-logs-once'"; \
			exit 1; \
		fi; \
		echo "  Waiting for Memoria... ($$i/60)"; \
		sleep 2; \
	done

.PHONY: dev-db-connect
dev-db-connect:
	@set -a; [ -f .env ] && . ./.env; set +a; \
	scripts/dev/mysql-client.sh

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

.PHONY: dev-api-start-debug
dev-api-start-debug:
	@BUILD_MODE=debug ./scripts/dev/start-api.sh

.PHONY: dev-api-restart
dev-api-restart: dev-api-stop
	@sleep 1
	@$(MAKE) dev-api-start

.PHONY: dev-api-restart-debug
dev-api-restart-debug: dev-api-stop
	@sleep 1
	@$(MAKE) dev-api-start-debug

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
		API_PORT=$${ASTRA_API_PORT:-$(DEFAULT_API_PORT)}; \
		if command -v jq >/dev/null 2>&1; then \
			NO_PROXY=localhost curl -s http://localhost:$$API_PORT/health 2>/dev/null | jq . || echo "  ⚠️  Health check failed"; \
		else \
			NO_PROXY=localhost curl -s http://localhost:$$API_PORT/health 2>/dev/null || echo "  ⚠️  Health check failed"; \
		fi; \
	else \
		echo "  ❌ Not running"; \
	fi

# ============================================================================
# Web UI (Next.js Dev Server)
# ============================================================================

.PHONY: dev-sdk-deps
dev-sdk-deps:
	@if [ ! -x packages/sdk/node_modules/.bin/tsup ]; then \
		echo "Installing local @astra/sdk dependencies..."; \
		cd packages/sdk && npm ci --no-audit --no-fund; \
	else \
		echo "✅ Local @astra/sdk dependencies ready"; \
	fi
	@if [ ! -f packages/sdk/dist/index.js ] || [ ! -f packages/sdk/dist/index.d.ts ]; then \
		echo "Building local @astra/sdk package..."; \
		cd packages/sdk && npm run build; \
	else \
		echo "✅ Local @astra/sdk build ready"; \
	fi

.PHONY: dev-web-deps
dev-web-deps: dev-sdk-deps
	@if [ ! -f web/node_modules/next/dist/bin/next ]; then \
		echo "Installing web UI dependencies..."; \
		cd web && npm ci --no-audit --no-fund; \
	else \
		echo "✅ Web UI dependencies ready"; \
	fi

.PHONY: dev-web-start
dev-web-start: dev-web-deps
	@./scripts/dev/start-web.sh

.PHONY: dev-web-stop
dev-web-stop:
	@./scripts/dev/stop-web.sh

.PHONY: dev-web-restart
dev-web-restart: dev-web-stop
	@sleep 1
	@$(MAKE) dev-web-start

.PHONY: dev-web-logs
dev-web-logs:
	@LOG_FILE=$${ASTRA_WEB_LOG_FILE:-web_server.log}; \
	if [ -f "$$LOG_FILE" ]; then \
		tail -f "$$LOG_FILE"; \
	else \
		echo "❌ $$LOG_FILE not found. Is web UI running?"; \
	fi

.PHONY: dev-web-status
dev-web-status:
	@PID_FILE=$${ASTRA_WEB_PID_FILE:-web_server.pid}; \
	WEB_PORT=$${ASTRA_WEB_PORT:-$${WEB_PORT:-3536}}; \
	echo "Web UI Status:"; \
	echo "=============="; \
	if [ -f "$$PID_FILE" ] && kill -0 $$(cat "$$PID_FILE") 2>/dev/null; then \
		PID=$$(cat "$$PID_FILE"); \
		RUNNING_PORT=$$(ps -p "$$PID" -o command= 2>/dev/null | sed -nE 's/.*--port[[:space:]]+([0-9]+).*/\1/p' | tail -1); \
		if [ -n "$$RUNNING_PORT" ] && [ "$$RUNNING_PORT" != "$$WEB_PORT" ]; then \
			echo "  ⚠️  Running on different port (PID: $$PID, URL: http://localhost:$$RUNNING_PORT; configured: $$WEB_PORT)"; \
		else \
			echo "  ✅ Running (PID: $$PID, URL: http://localhost:$$WEB_PORT)"; \
			NO_PROXY=localhost,127.0.0.1 curl -s --connect-timeout 1 --max-time 2 "http://127.0.0.1:$$WEB_PORT" >/dev/null 2>&1 || echo "  ⚠️  HTTP check failed"; \
		fi; \
	else \
		echo "  ❌ Not running"; \
	fi

# ============================================================================
# Edge Provider (local execution provider for server/web runtime)
# ============================================================================

.PHONY: dev-edge-start
dev-edge-start:
	@BUILD_MODE=$${ASTRA_EDGE_BUILD_MODE:-debug} ./scripts/dev/start-edge.sh

.PHONY: dev-edge-stop
dev-edge-stop:
	@./scripts/dev/stop-edge.sh

.PHONY: dev-edge-logs
dev-edge-logs:
	@LOG_FILE=$${ASTRA_EDGE_LOG_FILE:-$(CURDIR)/astra_edge.log}; \
	if [ -f "$$LOG_FILE" ]; then \
		tail -f "$$LOG_FILE"; \
	else \
		echo "❌ $$LOG_FILE not found. Is astra-edge running?"; \
	fi

.PHONY: dev-edge-status
dev-edge-status:
	@PID_FILE=$${ASTRA_EDGE_PID_FILE:-$(CURDIR)/astra_edge.pid}; \
	echo "Edge Provider Status:"; \
	echo "====================="; \
	if [ -f "$$PID_FILE" ] && kill -0 $$(cat "$$PID_FILE") 2>/dev/null; then \
		PID=$$(cat "$$PID_FILE"); \
		echo "  ✅ Running (PID: $$PID)"; \
	else \
		echo "  ❌ Not running"; \
	fi

# ============================================================================
# API Server (Docker Mode)
# ============================================================================

.PHONY: dev-api-docker-build
dev-api-docker-build:
	@echo "Building API server image..."
	@docker build $(DOCKER_PROXY_BUILD_ARGS) $(DOCKER_METADATA_BUILD_ARGS) $(DOCKER_BUILD_ARGS) -t $(IMAGE_NAME):latest .
	@echo "✅ Image built"

.PHONY: dev-api-docker-up
dev-api-docker-up:
	@echo "Starting API server (Docker mode)..."
	@cd deployment/all-in-one && docker compose --env-file ../../.env up -d --no-deps api
	@echo "✅ API server container started"

.PHONY: dev-api-docker-down
dev-api-docker-down:
	@echo "Stopping API server containers..."
	@cd deployment/all-in-one && docker compose --env-file ../../.env stop api && docker compose --env-file ../../.env rm -f api
	@echo "✅ API server containers stopped"

.PHONY: dev-api-docker-logs
dev-api-docker-logs:
	@cd deployment/all-in-one && docker compose --env-file ../../.env logs -f api

.PHONY: dev-api-docker-scale
dev-api-docker-scale:
	@if [ -z "$(REPLICAS)" ]; then \
		echo "❌ Usage: make dev-api-docker-scale REPLICAS=N"; \
		exit 1; \
	fi
	@echo "Scaling API server to $(REPLICAS) replicas..."
	@cd deployment/all-in-one && docker compose --env-file ../../.env up -d --no-deps --scale api=$(REPLICAS) api
	@echo "✅ Scaled to $(REPLICAS) replicas"

.PHONY: release-docker
release-docker:
	@if [ -z "$(VERSION)" ]; then \
		echo "❌ VERSION is required, for example: make release-docker VERSION=0.1.0 CONFIRM=yes"; \
		exit 1; \
	fi
	@if [ "$(CONFIRM)" != "yes" ]; then \
		echo "❌ Refusing to push Docker image without explicit confirmation."; \
		echo "   Run: make release-docker VERSION=$(VERSION) CONFIRM=yes"; \
		exit 1; \
	fi
	@scripts/validate-release-version.sh "$(VERSION)"
	@if [ "$(IMAGE_SOURCE_DIRTY)" != "false" ]; then \
		echo "❌ Refusing to publish from a dirty tracked worktree"; \
		exit 1; \
	fi
	@if [ "$$(git rev-parse HEAD)" != "$$(git rev-list -n 1 "v$(IMAGE_VERSION)" 2>/dev/null)" ]; then \
		echo "❌ HEAD must be the commit tagged v$(IMAGE_VERSION)"; \
		exit 1; \
	fi
	@echo "Building Docker image $(IMAGE_NAME):$(IMAGE_VERSION)..."
	@docker build $(DOCKER_PROXY_BUILD_ARGS) $(DOCKER_METADATA_BUILD_ARGS) $(DOCKER_BUILD_ARGS) -t "$(IMAGE_NAME):$(IMAGE_VERSION)" .
	@docker push "$(IMAGE_NAME):$(IMAGE_VERSION)"
	@if echo "$(IMAGE_VERSION)" | grep -q -- '-'; then \
		echo "Pre-release $(IMAGE_VERSION): leaving latest unchanged"; \
	else \
		docker tag "$(IMAGE_NAME):$(IMAGE_VERSION)" "$(IMAGE_NAME):latest"; \
		docker push "$(IMAGE_NAME):latest"; \
	fi
	@echo "✅ Pushed Docker image $(IMAGE_NAME):$(IMAGE_VERSION)"

# ============================================================================
# Compose Stack Deployment
# ============================================================================

.PHONY: stack-env
stack-env:
	@if [ -f "$(STACK_ENV)" ]; then \
		echo "✅ $(STACK_ENV) already exists"; \
	else \
		cp $(STACK_DIR)/.env.example $(STACK_ENV); \
		echo "✅ Created $(STACK_ENV)"; \
	fi; \
	if ! command -v openssl >/dev/null 2>&1; then \
		echo "❌ openssl is required to generate stack secrets"; \
		exit 1; \
	fi; \
	. scripts/lib/env_file.sh; \
	set_env_value() { \
		key="$$1"; \
		value="$$2"; \
		tmp="$$(mktemp)"; \
		awk -v key="$$key" -v value="$$value" ' \
			BEGIN { done = 0 } \
			{ \
				line = $$0; \
				sub(/^[[:space:]]*/, "", line); \
				if (line ~ "^" key "[[:space:]]*=") { \
					print key "=" value; \
					done = 1; \
					next; \
				} \
				print; \
			} \
			END { if (!done) print key "=" value } \
		' "$(STACK_ENV)" > "$$tmp"; \
		mv "$$tmp" "$(STACK_ENV)"; \
	}; \
	ensure_secret() { \
		key="$$1"; \
		if env_file_has_configured_value "$(STACK_ENV)" "$$key"; then \
			echo "✅ $$key already configured"; \
			return 0; \
		fi; \
		value="$$(openssl rand -hex 32)"; \
		set_env_value "$$key" "$$value"; \
		echo "✅ Generated $$key"; \
	}; \
	ensure_secret ASTRA_JWT_SECRET; \
	ensure_secret ASTRA_TOKEN_ENCRYPTION_KEY; \
	ensure_secret ASTRA_RUNTIME_ROOT_SECRET; \
	ensure_secret MEMORIA_MASTER_KEY; \
	echo "Configure a real embedding endpoint, or set MEMORIA_EMBEDDING_PROVIDER=mock for local evaluation, before running: make stack-up"

.PHONY: stack-check-env
stack-check-env:
	@if [ ! -f "$(STACK_ENV)" ]; then \
		echo "❌ Missing $(STACK_ENV)"; \
		echo "   Run: make stack-env"; \
		exit 1; \
	fi
	@. scripts/lib/env_file.sh; \
	embedding_provider="$$(env_file_read "$(STACK_ENV)" MEMORIA_EMBEDDING_PROVIDER 2>/dev/null || true)"; \
	embedding_provider="$$(printf '%s' "$$embedding_provider" | tr '[:upper:]' '[:lower:]')"; \
	required="$(STACK_SECRET_ENV)"; \
	if [ "$${embedding_provider:-openai}" != "mock" ]; then \
		required="$$required $(STACK_EMBEDDING_ENV)"; \
	fi; \
	missing=""; \
	for key in $$required; do \
		if ! env_file_has_configured_value "$(STACK_ENV)" "$$key"; then \
			missing="$$missing $$key"; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "❌ Missing or insecure required config in $(STACK_ENV):$$missing"; \
		echo "   Run make stack-env to generate secrets. For non-mock embeddings, fill the base URL and any provider-required API key."; \
		exit 1; \
	fi

.PHONY: stack-config
stack-config: stack-check-env
	@$(STACK_COMPOSE) config --quiet
	@echo "✅ Compose stack config OK"

.PHONY: stack-up
stack-up: stack-config
	@echo "Starting compose stack..."
	@$(STACK_COMPOSE) up -d --wait --wait-timeout 180
	@echo "✅ Compose stack started"
	@API_PORT=$$(sed -n 's/^ASTRA_API_PORT=//p' $(STACK_ENV) | tail -1); \
	echo "   API: http://localhost:$${API_PORT:-$(DEFAULT_API_PORT)}"

.PHONY: stack-up-server-only
stack-up-server-only:
	@echo "Ensuring no local astra-edge provider remains connected..."
	@$(MAKE) dev-edge-stop
	@$(MAKE) stack-up
	@echo "✅ Server-only stack ready (no local edge provider connected)"

.PHONY: stack-up-server-edge
stack-up-server-edge: stack-up-server-only
	@API_PORT=$$([ -f "$(STACK_ENV)" ] && sed -n 's/^ASTRA_API_PORT=//p' $(STACK_ENV) | tail -1 || true); \
	ASTRA_EDGE_SERVER_URL="http://127.0.0.1:$${API_PORT:-$(DEFAULT_API_PORT)}" $(MAKE) dev-edge-start
	@echo "✅ Server + edge stack ready"

.PHONY: stack-down
stack-down:
	@$(STACK_COMPOSE) down
	@echo "✅ Compose stack stopped"

.PHONY: stack-clean
stack-clean:
	@$(STACK_COMPOSE) down -v
	@echo "✅ Compose stack stopped and volumes removed"

.PHONY: stack-status
stack-status: stack-check-env
	@$(STACK_COMPOSE) ps

.PHONY: stack-logs
stack-logs: stack-check-env
	@$(STACK_COMPOSE) logs -f $(SERVICE)

# ============================================================================
# Composite Commands
# ============================================================================

.PHONY: dev-start-server-only
dev-start-server-only:
	@echo "Starting server-only development environment..."
	@$(MAKE) dev-edge-stop
	@$(MAKE) dev-deps-up
	@$(MAKE) dev-deps-wait
	@$(MAKE) dev-api-start
	@$(MAKE) dev-web-start
	@echo ""
	@echo "✅ Server-only development environment started!"
	@echo "   API: http://localhost:$${ASTRA_API_PORT:-$(DEFAULT_API_PORT)}"
	@echo "   Web: http://localhost:$${ASTRA_WEB_PORT:-$${WEB_PORT:-3536}}"
	@echo "   Edge provider: not connected"
	@echo ""
	@echo "Next steps:"
	@echo "  astra register"
	@echo "  astra login"
	@echo "  astra chat"

.PHONY: dev-start-server-edge
dev-start-server-edge:
	@$(MAKE) dev-start-server-only
	@$(MAKE) dev-edge-start
	@echo ""
	@echo "✅ Server + edge development environment started!"
	@echo "   API: http://localhost:$${ASTRA_API_PORT:-$(DEFAULT_API_PORT)}"
	@echo "   Web: http://localhost:$${ASTRA_WEB_PORT:-$${WEB_PORT:-3536}}"
	@echo "   Edge workspace: $${ASTRA_EDGE_WORKSPACE_DIR:-$$(pwd)}"

.PHONY: dev-start
dev-start:
	@echo "Starting development environment..."
	@$(MAKE) dev-deps-up
	@$(MAKE) dev-deps-wait
	@$(MAKE) dev-api-start
	@$(MAKE) dev-web-start
	@echo ""
	@echo "✅ Development environment started!"
	@echo "   API: http://localhost:$${ASTRA_API_PORT:-$(DEFAULT_API_PORT)}"
	@echo "   Web: http://localhost:$${ASTRA_WEB_PORT:-$${WEB_PORT:-3536}}"
	@echo "   Edge provider: unchanged"
	@echo ""
	@echo "Use make dev-start-server-only to explicitly disconnect local edge."
	@echo "Use make dev-start-server-edge to start or reconnect local edge."

.PHONY: dev-start-docker
# Docker API mode reuses the dev dependency stack; dev-api-docker-up starts only api.
dev-start-docker: dev-deps-up dev-deps-wait dev-api-docker-up
	@sleep 3
	@echo ""
	@echo "✅ Development environment ready (Docker mode)!"
	@echo "   API: http://localhost:$${ASTRA_API_PORT:-$(DEFAULT_API_PORT)}"

.PHONY: dev-stop
dev-stop: dev-edge-stop dev-web-stop dev-api-stop dev-deps-down
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
	@echo ""
	@$(MAKE) dev-web-status
	@echo ""
	@$(MAKE) dev-edge-status

.PHONY: dev-clean
dev-clean: dev-edge-stop dev-web-stop dev-api-stop dev-deps-clean
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
	DB_NAME=$${ASTRA_DATABASE:-astra_runtime}; \
	SQL="DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;"; \
	scripts/dev/mysql-client.sh -e "$$SQL"
	@$(MAKE) dev-api-restart-debug build-cli-debug
	@sleep 2
	@echo "Registering admin (admin@mo.com)..."
	@NO_PROXY=localhost ./target/debug/astra admin register \
		--username admin --password 11111111 --email admin@mo.com
	@echo "Logging in as admin..."
	@NO_PROXY=localhost ./target/debug/astra admin login \
		--username admin --password 11111111
	@echo "Loading models from .models.yaml..."
	@NO_PROXY=localhost ./target/debug/astra admin model load .models.yaml --update-existing
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
	@echo "✅ Release artifacts: $(RUST_RELEASE_BIN_DIR)/"

.PHONY: build-cli
build-cli: build-cli-release

.PHONY: build-cli-release
build-cli-release: sweep
	@echo "Building astra CLI (release)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-cli --release --bin astra $(CLI_RELEASE_FLAGS)
	@echo "Binaries:"
	@for bin in $(CLI_BINS); do echo "  $(RUST_RELEASE_BIN_DIR)/$$bin"; done

.PHONY: build-server
build-server: build-server-release

.PHONY: build-server-release
build-server-release: sweep
	@echo "Building astra-server (release)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --release --bin $(API_SERVER_BIN)
	@echo "Binary: $(RUST_RELEASE_BIN_DIR)/$(API_SERVER_BIN)"

.PHONY: build-edge
build-edge: build-edge-release

.PHONY: build-edge-release
build-edge-release: sweep
	@echo "Building astra-edge (release)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-edge --release --bin $(EDGE_BIN)
	@echo "Binary: $(RUST_RELEASE_BIN_DIR)/$(EDGE_BIN)"

# --- Debug builds (no sweep, no --release => fast incremental) ---

.PHONY: build-debug
build-debug:
	@echo "Building Rust workspace (debug)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG)
	@echo "✅ Debug artifacts: $(RUST_DEBUG_BIN_DIR)/"

.PHONY: build-cli-debug
build-cli-debug:
	@echo "Building astra CLI (debug)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-cli --bin astra
	@echo "Binaries:"
	@for bin in $(CLI_BINS); do echo "  $(RUST_DEBUG_BIN_DIR)/$$bin"; done

.PHONY: build-server-debug
build-server-debug:
	@echo "Building astra-server (debug)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --bin $(API_SERVER_BIN)
	@echo "Binary: $(RUST_DEBUG_BIN_DIR)/$(API_SERVER_BIN)"

.PHONY: build-edge-debug
build-edge-debug:
	@echo "Building astra-edge (debug)..."
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-edge --bin $(EDGE_BIN)
	@echo "Binary: $(RUST_DEBUG_BIN_DIR)/$(EDGE_BIN)"

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

# Deep cleanup: removes artifacts not accessed in STALE_HOURS hours.
# Unlike `sweep` (time-based on mtime, runs automatically), this is manual
# and aggressive — good for reclaiming disk when target/ has ballooned.
# Override: make clean-stale STALE_HOURS=6
STALE_HOURS ?= 12

.PHONY: clean-stale
clean-stale:
	@echo "Current target/ size:"; du -sh $(RUST_TARGET_DIR) 2>/dev/null || true
	@echo ""
	@mkdir -p $(RUST_TARGET_DIR)/debug $(RUST_TARGET_DIR)/release
	@if ! command -v flock >/dev/null 2>&1; then \
		echo "Skipping stale cleanup: flock is unavailable on this platform"; \
		exit 0; \
	fi; \
	exec 8>$(RUST_TARGET_DIR)/debug/.cargo-lock; \
	exec 9>$(RUST_TARGET_DIR)/release/.cargo-lock; \
	if ! flock -n 8 || ! flock -n 9; then \
		echo "Skipping stale cleanup: Cargo is using the target directory"; \
		exit 0; \
	fi; \
	echo "Removing artifacts not accessed in $(STALE_HOURS)h..."; \
	STALE_MIN=$$(( $(STALE_HOURS) * 60 )); \
	for PROFILE in debug release; do \
		DIR=$(RUST_TARGET_DIR)/$$PROFILE; \
		[ -d "$$DIR" ] || continue; \
		find $$DIR/incremental -mindepth 1 -maxdepth 1 -type d -amin +$$STALE_MIN -exec rm -rf {} + 2>/dev/null || true; \
		find $$DIR/deps -mindepth 1 -maxdepth 1 -amin +$$STALE_MIN -exec rm -rf {} + 2>/dev/null || true; \
		find $$DIR/.fingerprint -mindepth 1 -maxdepth 1 -type d -amin +$$STALE_MIN -exec rm -rf {} + 2>/dev/null || true; \
		find $$DIR/build -mindepth 1 -maxdepth 1 -type d -amin +$$STALE_MIN -exec rm -rf {} + 2>/dev/null || true; \
		find $$DIR/examples -type f -amin +$$STALE_MIN -delete 2>/dev/null || true; \
	done; \
	echo ""; \
	echo "After cleanup:"; du -sh $(RUST_TARGET_DIR) 2>/dev/null || true; \
	echo "✅ Stale artifacts (>$(STALE_HOURS)h) removed"

# Maximum age (in hours) for build artifacts before sweep removes them.
# Override: make sweep SWEEP_MAX_AGE_H=2
SWEEP_MAX_AGE_H ?= 4

# Time-based sweep: removes debug and release artifacts older than SWEEP_MAX_AGE_H.
# Runs automatically before build-release and test-offline to keep disk usage bounded.
.PHONY: sweep
sweep:
	@mkdir -p $(RUST_TARGET_DIR)/debug $(RUST_TARGET_DIR)/release
	@if ! command -v flock >/dev/null 2>&1; then \
		echo "Skipping artifact sweep: flock is unavailable on this platform"; \
		exit 0; \
	fi; \
	exec 8>$(RUST_TARGET_DIR)/debug/.cargo-lock; \
	exec 9>$(RUST_TARGET_DIR)/release/.cargo-lock; \
	if ! flock -n 8 || ! flock -n 9; then \
		echo "Skipping artifact sweep: Cargo is using the target directory"; \
		exit 0; \
	fi; \
	AGE_MIN=$$(( $(SWEEP_MAX_AGE_H) * 60 )); \
	echo "Sweeping artifacts older than $(SWEEP_MAX_AGE_H)h..."; \
	find $(RUST_TARGET_DIR)/debug/incremental -mindepth 1 -maxdepth 1 -type d -mmin +$$AGE_MIN -exec rm -rf {} + 2>/dev/null || true; \
	find $(RUST_TARGET_DIR)/debug/deps -mindepth 1 -maxdepth 1 -mmin +$$AGE_MIN -exec rm -rf {} + 2>/dev/null || true; \
	find $(RUST_TARGET_DIR)/debug/.fingerprint -mindepth 1 -maxdepth 1 -type d -mmin +$$AGE_MIN -exec rm -rf {} + 2>/dev/null || true; \
	find $(RUST_TARGET_DIR)/release/incremental -mindepth 1 -maxdepth 1 -type d -mmin +$$AGE_MIN -exec rm -rf {} + 2>/dev/null || true; \
	find $(RUST_TARGET_DIR)/release/deps -mindepth 1 -maxdepth 1 -mmin +$$AGE_MIN -exec rm -rf {} + 2>/dev/null || true; \
	find $(RUST_TARGET_DIR)/release/.fingerprint -mindepth 1 -maxdepth 1 -type d -mmin +$$AGE_MIN -exec rm -rf {} + 2>/dev/null || true; \
	echo "✅ Stale artifacts removed"; \
	du -sh $(RUST_TARGET_DIR) 2>/dev/null || true

# ============================================================================
# Testing
# ============================================================================

.PHONY: test test-offline test-online test-no-sticky-control test-saas test-saas-coverage test-sdk-offline test-web-offline test-sdk-online validate-capability-matrix
test: test-offline test-online

.PHONY: test-dashboard
test-dashboard: ## Build astra-test and launch live dashboard
	@cargo build --release -p astra-test-harness
	./target/release/astra-test --live-dashboard

.PHONY: test-offline
# Run the focused runtime profile gate first so provider/surface regressions fail
# before the broader workspace, server E2E-hook, SDK, and web offline suites.
test-offline: sweep validate-capability-matrix test-runtime-profiles test-workspace test-runtime-e2e-hooks test-sdk-offline test-web-offline

.PHONY: validate-capability-matrix
validate-capability-matrix:
	@python3 scripts/e2e/validate_capability_matrix.py

.PHONY: test-runtime-profiles
test-runtime-profiles: test-server-only test-server-edge test-managed-runtime test-cli-local
	@echo "✅ Runtime profile guardrails passed"

.PHONY: test-server-only
test-server-only:
	@echo "Running focused server-only access-surface tests..."
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime-env runtime_execution_provider_type_requires_matching_workspace_executor_pair
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --test runtime_refactor_guardrails
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::binding_resolution::tests::edge_tools_without_profile_do_not_create_provider_binding
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_binding_projection::tests::server_provider_surface_does_not_start_from_workspace_tools
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_binding_projection::tests::mismatched_workspace_executor_does_not_expose_runtime_tools
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_binding_projection::tests::explicit_offline_runtime_binding_does_not_expose_runtime_tools
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::visible_turn_tools_excludes_only_the_disabled_edge_offer
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_transport_metadata::tests::no_workspace_reports_sandbox_unbound
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_transport_metadata::tests::mismatched_workspace_executor_reports_unbound_provider
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_workspace_path_guard::tests::workspace_ownership_rejects_symlink_escape_even_when_lexical_path_is_inside
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::run_turn_pipeline_includes_turn_start_lifecycle_summary_for_web_agent
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::lifecycle::tests::runtime_manifest_preserves_server_only_backbone_without_workspace_executor
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::runtime_tool_executor::tests::server_only_introspect_json_preserves_provider_coverage_graph
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::runtime_tool_executor::tests::server_only_reflect_report_includes_runtime_provider_coverage
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::lifecycle::tests::stream_run_cache_miss_replays_durable_text_done
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::lifecycle::tests::subrun_turn_budget
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::lifecycle::tests::server_subrun_
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::lifecycle::tests::finalize_run_events
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --test web_agent_e2e --features e2e-hooks cli_thin_client_single_admission_completes_server_owned_multi_round_loop
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --test web_agent_e2e --features e2e-hooks web_agent_structured_spawn_waits_for_server_child_before_parent_synthesis
	@cd web && npm test -- --run \
		__tests__/app/edges-status-route.test.ts \
		__tests__/lib/chat-input-route.test.ts \
		__tests__/lib/chat-stream-route.test.ts \
		__tests__/lib/stream-event-handler.test.ts \
		__tests__/lib/workspace-authority.test.ts

.PHONY: test-server-edge
test-server-edge:
	@echo "Running focused server+edge provider tests..."
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-edge
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::binding_resolution::tests::request_bindings_without_server_workspace_require_typed_binding
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::run::lifecycle::tests::edge_profile_does_not_infer_execution_bindings
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::turn_start_lifecycle_summary_reports_edge_provider_binding
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::builder_composes_server_owned_tools_with_edge_declared_runtime_tools
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::offline_edge_blocking_does_not_require_sse_event_channel
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::edge_ledger_delivery_selects_only_edge_bound_runtime_tools
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::server_catalog_with_edge_binding_still_routes_runtime_tools_to_edge
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_binding_projection::tests::server_edge_composition_exposes_server_services_and_edge_runtime_tools
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_transport_metadata::tests::edge_workspace_without_server_cwd_reports_edge_capacity_ready
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_transport_metadata::tests::edge_offline_reports_edge_capacity_offline
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib edge_bound_selected_executor_does_not_route_to_other_connected_edge
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib edge_bound_offline_or_unknown_status_blocks_without_dispatch
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib edge_dispatch_without_result_reports_transport_disconnected
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --test edge_ws_e2e edge_ws_relay_strips_legacy_boundary_and_preserves_inflight_dispatch
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --test web_agent_e2e --features e2e-hooks web_agent_dynamic_spawn_inherits_edge_workspace_binding
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --test web_agent_e2e --features e2e-hooks edge_executor_offline_child_returns_actionable_wait_to_structured_parent
	@cd web && npm test -- --run \
		__tests__/app/edges-status-route.test.ts \
		__tests__/lib/work-surface.test.ts \
		__tests__/lib/workspace-authority.test.ts

.PHONY: test-managed-runtime
test-managed-runtime:
	@echo "Running focused sandbox, orchestrator-managed runtime, and request-scoped MCP provider tests..."
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime-env request_scoped_mcp_schema_filter_requires_exact_provider_binding
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime-env read_only_snapshot_helper_exposes_reads_through_orchestrator_runtime
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_binding_projection::tests::mcp_schema_is_hidden_without_request_scoped_mcp_provider
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_binding_projection::tests::mcp_executor_provider_declares_request_scoped_mcp_schemas
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_binding_projection::tests::server_sandbox_binding_exposes_project_runtime_tools
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::builder_runtime_surface_follows_server_sandbox_binding
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::builder_runtime_surface_follows_orchestrator_read_only_binding
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::server_loop_host::tests::tool_call_start_projects_request_scoped_mcp_route_metadata
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib server::tool_transport_metadata::tests::server_sandbox_reports_sandbox_ready
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib orchestrator_managed_executes_through_sandbox_resident_agent_transport
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib orchestrator_managed_without_sandbox_resident_agent_transport_does_not_reroute_to_local
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib request_scoped_mcp_tools_bypass_edge_transport

.PHONY: test-cli-local
test-cli-local:
	@echo "Running focused CLI-local provider tests..."
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-runtime --lib capabilities::tests::cli_local_catalog_filters_builtin_source_by_provider_ownership
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-cli --lib edge_tools::tests::schema_tests::local_cli_catalog_exposes_the_root_work_lifecycle

# Fast correctness gate for the no-sticky control plane. This intentionally
# stays out of the default test targets: it is focused evidence for LB/session
# affinity removal, not a replacement for live multi-pod deployment validation.
.PHONY: test-no-sticky-control
test-no-sticky-control:
	@echo "Running no-sticky control-plane tests (approval, ask_user, edge callbacks; no live DB required)..."
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib replays_from_journal -- --nocapture
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --lib do_not_require_sticky_pod -- --nocapture
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) --test edge_5_5_http_e2e without_sticky_ledger -- --nocapture

.PHONY: test-workspace
test-workspace: sweep
	@echo "Running Rust workspace tests (nextest profile=$(NEXTEST_OFFLINE_PROFILE))..."
	@CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) --workspace $(NEXTEST_OFFLINE_FLAGS)
	@echo "Running workspace doctests (cargo test --doc; not covered by nextest)..."
	@CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) --workspace --doc

# Compiles deterministic model/edge hooks used by runtime system journeys.
.PHONY: test-runtime-e2e-hooks
test-runtime-e2e-hooks: sweep
	@echo "Running astra-runtime tests with feature e2e-hooks (nextest profile=$(NEXTEST_OFFLINE_PROFILE))..."
	@CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) \
		--features e2e-hooks $(NEXTEST_OFFLINE_FLAGS)

# Ignored tests: opt-in via env vars (see `make test-online`). Enable with:
#   ASTRA_TEST_DB_IT=1   -> all online/Matrix ignored integration tests (--ignored)
#
# This helper owns the astra-runtime / astra-plan integration binaries.
# astra-services integration binaries run in test-online's core lane, beside
# the astra-services ignored lib tests, so their test-binary linking and live-DB
# time are charged to core. `make test-online` remains the complete online suite.
#
# Optional serial mode: ASTRA_TEST_DB_IT_TEST_THREADS=1 -> -j 1
.PHONY: test-ignored-integration
test-ignored-integration:
	@if [ "$${ASTRA_TEST_DB_IT:-}" != "1" ]; then \
		echo "Note: no online/Matrix ignored suites selected. Use \`make test-online\` or set ASTRA_TEST_DB_IT=1."; \
	fi
	@if [ "$${ASTRA_TEST_DB_IT:-}" = "1" ]; then \
		FAILED=""; \
		JOBS_FLAG=""; \
		if [ "$${ASTRA_TEST_DB_IT_TEST_THREADS:-}" = "1" ]; then \
			JOBS_FLAG="-j 1"; \
			echo "Online integration tests: serial mode (ASTRA_TEST_DB_IT_TEST_THREADS=1)"; \
		else \
			echo "Running online integration tests (ignored; live MatrixOne; e2e-hooks enabled for system_matrix_http_e2e)..."; \
		fi; \
		PERF_FAILED=""; \
		if [ "$$JOBS_FLAG" = "-j 1" ] && [ "$${ASTRA_STRICT_ONLINE_PERF:-1}" != "0" ]; then \
			echo "Running runtime/plan integration and performance tests in one serial build..."; \
			RUST_MIN_STACK=$${RUST_MIN_STACK:-16777216} ASTRA_RUNTIME_ROOT_SECRET=$${ASTRA_RUNTIME_ROOT_SECRET:-test-runtime-root-secret} ASTRA_TEST_E2E_SECRET=$${ASTRA_TEST_E2E_SECRET:-system-matrix-e2e-secret} ASTRA_BACKEND_SERVICE_KEY=$${ASTRA_BACKEND_SERVICE_KEY:-test-service-key-e2e} ASTRA_LLM_RETRY_BASE_MS=$${ASTRA_LLM_RETRY_BASE_MS:-10} ASTRA_DEFAULT_RETRY_AFTER_MS=$${ASTRA_DEFAULT_RETRY_AFTER_MS:-10} ASTRA_BCRYPT_COST=$${ASTRA_BCRYPT_COST:-4} CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) \
				-p astra-runtime -p astra-plan \
				--features astra-runtime/e2e-hooks \
				--tests --run-ignored only \
				$(NEXTEST_ONLINE_FLAGS) $$JOBS_FLAG \
				-E '$(NEXTEST_PHASE0_BASELINE_EXCLUSION)' \
					|| FAILED="$$FAILED runtime-plan-perf"; \
		else \
			RUST_MIN_STACK=$${RUST_MIN_STACK:-16777216} ASTRA_RUNTIME_ROOT_SECRET=$${ASTRA_RUNTIME_ROOT_SECRET:-test-runtime-root-secret} ASTRA_TEST_E2E_SECRET=$${ASTRA_TEST_E2E_SECRET:-system-matrix-e2e-secret} ASTRA_BACKEND_SERVICE_KEY=$${ASTRA_BACKEND_SERVICE_KEY:-test-service-key-e2e} ASTRA_LLM_RETRY_BASE_MS=$${ASTRA_LLM_RETRY_BASE_MS:-10} ASTRA_DEFAULT_RETRY_AFTER_MS=$${ASTRA_DEFAULT_RETRY_AFTER_MS:-10} ASTRA_BCRYPT_COST=$${ASTRA_BCRYPT_COST:-4} CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) \
				-p astra-runtime -p astra-plan \
				--features astra-runtime/e2e-hooks \
				--tests --run-ignored only \
				$(NEXTEST_ONLINE_FLAGS) $$JOBS_FLAG \
				-E 'not binary(perf_benchmarks) and $(NEXTEST_PHASE0_BASELINE_EXCLUSION)' \
					|| FAILED="$$FAILED integration"; \
			echo "Running online performance benchmarks in an isolated serial lane (blocking unless ASTRA_STRICT_ONLINE_PERF=0)..."; \
			CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) \
				-p astra-runtime \
				--features astra-runtime/e2e-hooks \
				--tests --run-ignored only \
				$(NEXTEST_ONLINE_FLAGS) -j 1 \
				-E 'binary(perf_benchmarks)' \
					|| PERF_FAILED=1; \
		fi; \
		if [ -n "$$FAILED" ]; then \
			echo "❌ test-ignored-integration: failed lanes:$$FAILED"; \
			exit 1; \
		fi; \
		if [ -n "$$PERF_FAILED" ]; then \
			if [ "$${ASTRA_STRICT_ONLINE_PERF:-}" = "0" ]; then \
				echo "WARNING: online perf lane failed; continuing because ASTRA_STRICT_ONLINE_PERF=0"; \
			else \
				echo "❌ test-ignored-integration: online perf lane failed (set ASTRA_STRICT_ONLINE_PERF=0 to opt out)"; \
				exit 1; \
			fi; \
		fi; \
	fi

# Online (MatrixOne): opt-in #[ignore] integration binaries (see test-ignored-integration).
# @astra/sdk remote E2E is opt-in (ASTRA_SDK_ONLINE_E2E=1) so CI make test-online has no API on :$(DEFAULT_API_PORT).
# Args for test-online's helper `run_mysql_ddl`: $$1 = SQL to execute; remaining args = optional mysql flags.
.PHONY: test-online
test-online:
	@if [ ! -f .env ]; then \
		echo "No .env found — creating from .env.example..."; \
		cp .env.example .env; \
	fi
	@set -a; [ -f .env ] && . ./.env; set +a; \
	TEST_DB_BASE=$${ASTRA_TEST_DATABASE:-astra_runtime_test}; \
	RUNTIME_IGNORED_DB="$${TEST_DB_BASE}_runtime_ignored"; \
	INTEGRATION_DB="$${TEST_DB_BASE}_integration"; \
	ONLINE_LANE=$${ASTRA_ONLINE_LANE:-all}; \
	ONLINE_JOBS_FLAG=""; \
	if [ "$${ASTRA_TEST_DB_IT_TEST_THREADS:-}" = "1" ]; then ONLINE_JOBS_FLAG="-j 1"; fi; \
	case "$$ONLINE_LANE" in \
		all) DB_NAMES="$$RUNTIME_IGNORED_DB $$INTEGRATION_DB" ;; \
		core) DB_NAMES="$$RUNTIME_IGNORED_DB" ;; \
		integration) DB_NAMES="$$INTEGRATION_DB" ;; \
		*) echo "❌ invalid ASTRA_ONLINE_LANE=$$ONLINE_LANE (expected all, core, or integration)"; exit 2 ;; \
	esac; \
	echo "Running online lane=$$ONLINE_LANE; recreating test databases: $$DB_NAMES ..."; \
	for DB_NAME in $$DB_NAMES; do \
		SQL="DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;"; \
		scripts/dev/mysql-client.sh -e "$$SQL" 2>/dev/null || true; \
	done; \
	FAILED=""; \
	if [ "$$ONLINE_LANE" != "integration" ]; then \
		echo "Running astra-runtime ignored unit/bin tests (live DB=$$RUNTIME_IGNORED_DB; nextest profile=$(NEXTEST_ONLINE_PROFILE); live-LLM suite gated by ASTRA_LIVE_LLM)..."; \
		ASTRA_DATABASE=$$RUNTIME_IGNORED_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
			ASTRA_TEST_DB_IT=1 \
			CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) $(API_SHELL_PKG) \
				--lib --bins --run-ignored only $(NEXTEST_ONLINE_FLAGS) $$ONLINE_JOBS_FLAG \
				-E 'not test(/durable_run_event_pressure_probe/) and $(NEXTEST_CLEANUP_PRESSURE_EXCLUSION)' \
				|| FAILED="$$FAILED astra-runtime-ignored"; \
		echo "Running astra-turn-core db-store ignored tests (live DB=$$RUNTIME_IGNORED_DB; nextest profile=$(NEXTEST_ONLINE_PROFILE))..."; \
		ASTRA_DATABASE=$$RUNTIME_IGNORED_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
			ASTRA_TEST_DB_IT=1 \
			CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) -p astra-turn-core \
				--features db-store --lib --run-ignored only $(NEXTEST_ONLINE_FLAGS) $$ONLINE_JOBS_FLAG \
				-E '$(NEXTEST_CLEANUP_PRESSURE_EXCLUSION)' \
				|| FAILED="$$FAILED astra-turn-core-db-store"; \
		echo "Running astra-services ignored lib/integration tests (live DB=$$RUNTIME_IGNORED_DB; nextest profile=$(NEXTEST_ONLINE_PROFILE))..."; \
		ASTRA_DATABASE=$$RUNTIME_IGNORED_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
			ASTRA_TEST_DATABASE=$$RUNTIME_IGNORED_DB \
			ASTRA_TEST_DB_IT=1 \
			CARGO_INCREMENTAL=0 cargo nextest run $(CARGO_MANIFEST_FLAG) -p astra-services \
				--lib --tests --run-ignored only $(NEXTEST_ONLINE_FLAGS) $$ONLINE_JOBS_FLAG \
				-E '$(NEXTEST_CLEANUP_PRESSURE_EXCLUSION)' \
				|| FAILED="$$FAILED astra-services-online"; \
	fi; \
	if [ "$$ONLINE_LANE" != "core" ]; then \
		echo "Running ignored integration suites (live DB=$$INTEGRATION_DB; nextest profile=$(NEXTEST_ONLINE_PROFILE))..."; \
		ASTRA_DATABASE=$$INTEGRATION_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
			ASTRA_TEST_DATABASE=$$INTEGRATION_DB \
			ASTRA_TEST_DB_IT=1 \
			ASTRA_TEST_E2E_SECRET=$${ASTRA_TEST_E2E_SECRET:-system-matrix-e2e-secret} \
			ASTRA_BACKEND_SERVICE_KEY=$${ASTRA_BACKEND_SERVICE_KEY:-test-service-key-e2e} \
			ASTRA_LLM_RETRY_BASE_MS=$${ASTRA_LLM_RETRY_BASE_MS:-10} \
			ASTRA_DEFAULT_RETRY_AFTER_MS=$${ASTRA_DEFAULT_RETRY_AFTER_MS:-10} \
			ASTRA_BCRYPT_COST=$${ASTRA_BCRYPT_COST:-4} \
			RUST_MIN_STACK=$${RUST_MIN_STACK:-16777216} \
			$(MAKE) test-ignored-integration \
			|| FAILED="$$FAILED test-ignored-integration"; \
	fi; \
	if [ -n "$$FAILED" ]; then \
		echo "❌ test-online: failed suites:$$FAILED"; \
		exit 1; \
	fi
	@if [ "$${ASTRA_ONLINE_LANE:-all}" = "core" ]; then \
		echo "Skipping @astra/sdk remote E2E in the core online lane"; \
	elif [ "$${ASTRA_SDK_ONLINE_E2E:-}" = "1" ]; then \
		$(MAKE) test-sdk-online; \
	else \
		echo "Skipping @astra/sdk remote E2E (set ASTRA_SDK_ONLINE_E2E=1 with API running, or: make test-sdk-online)"; \
	fi
	@if [ "$${ASTRA_MEMORIA_ONLINE:-}" = "1" ]; then \
		$(MAKE) test-memoria-online-contract; \
	else \
		echo "Skipping real Memoria contract (set ASTRA_MEMORIA_ONLINE=1, or: make test-memoria-online-contract)"; \
	fi
	@echo ""
	@echo "NOTE: live-LLM suite (real provider APIs, reads .models.yaml) auto-skips unless"
	@echo "      ASTRA_LIVE_LLM=1 is set. Run it explicitly with: make test-live-llm"

# Explicit cleanup pressure probes. This is intentionally not part of
# test-online because pressure timings are operational evidence, not a normal
# per-case correctness budget.
.PHONY: test-memoria-online-contract
test-memoria-online-contract:
	@if [ ! -f .env ]; then echo "❌ .env is required for the real Memoria contract"; exit 2; fi
	@set -a; . ./.env; set +a; \
		if [ -z "$$MEMORIA_MASTER_KEY" ]; then \
			echo "❌ MEMORIA_MASTER_KEY is required for the real Memoria contract"; exit 2; \
		fi; \
		ASTRA_MEMORIA_ONLINE=1 $(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-tools \
			--test memoria_online_contract -- --ignored

.PHONY: test-cleanup-pressure
test-cleanup-pressure:
	@python3 scripts/load/cleanup_pressure_probe.py \
		--profile "$(CLEANUP_PRESSURE_PROFILE)" \
		--database-base "$(CLEANUP_PRESSURE_DATABASE_BASE)" \
		$(CLEANUP_PRESSURE_ARGS)

.PHONY: test-durable-event-pressure
test-durable-event-pressure:
	@python3 scripts/load/durable_event_pressure_probe.py \
		--profile "$(DURABLE_EVENT_PRESSURE_PROFILE)" \
		--database "$(DURABLE_EVENT_PRESSURE_DATABASE)" \
		$(DURABLE_EVENT_PRESSURE_ARGS)

# SaaS platform E2E (docs/testing/saas-test-plan.md §5): resource governance, admin RBAC,
# auth refresh, session reaper. Requires MatrixOne + .env secrets (same as test-online).
# Serial (--test-threads=1): parallel runs share astra_runtime_test and each bootstrap
# calls recover_active_runs(), which marks other tests' in-flight runs as failed.
.PHONY: test-saas
test-saas:
	@if [ ! -f .env ]; then \
		echo "No .env found — creating from .env.example..."; \
		cp .env.example .env; \
	fi
	@set -a; [ -f .env ] && . ./.env; set +a; \
	TEST_DB=$${ASTRA_TEST_DATABASE:-astra_runtime_test}; \
	echo "Running SaaS platform E2E (ASTRA_TEST_DB_IT=1, database=$$TEST_DB, --test-threads=1)..."; \
	ASTRA_DATABASE=$$TEST_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
	ASTRA_TEST_DB_IT=1 ASTRA_TEST_E2E_SECRET=$${ASTRA_TEST_E2E_SECRET:-system-matrix-e2e-secret} \
	ASTRA_BACKEND_SERVICE_KEY=$${ASTRA_BACKEND_SERVICE_KEY:-test-service-key-e2e} \
	ASTRA_LLM_RETRY_BASE_MS=$${ASTRA_LLM_RETRY_BASE_MS:-10} \
	ASTRA_DEFAULT_RETRY_AFTER_MS=$${ASTRA_DEFAULT_RETRY_AFTER_MS:-10} \
	ASTRA_BCRYPT_COST=$${ASTRA_BCRYPT_COST:-4} \
	RUST_MIN_STACK=$${RUST_MIN_STACK:-16777216} \
	ASTRA_DB_POOL_MAX_CONNECTIONS=$${ASTRA_DB_POOL_MAX_CONNECTIONS:-5} \
	ASTRA_DB_GLOBAL_MAX_CONNECTIONS=$${ASTRA_DB_GLOBAL_MAX_CONNECTIONS:-10000} \
	CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) \
		-p astra-runtime \
		--features e2e-hooks \
		--test system_matrix_http_e2e \
		-- --ignored --nocapture e2e_matrix_saas_ --test-threads=1 \
	&& ASTRA_DATABASE=$$TEST_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
	ASTRA_TEST_DB_IT=1 \
	ASTRA_DB_POOL_MAX_CONNECTIONS=$${ASTRA_DB_POOL_MAX_CONNECTIONS:-5} \
	ASTRA_DB_GLOBAL_MAX_CONNECTIONS=$${ASTRA_DB_GLOBAL_MAX_CONNECTIONS:-10000} \
	CARGO_INCREMENTAL=0 $(CARGO) test $(CARGO_MANIFEST_FLAG) \
		-p astra-services \
		--test session_reaper_db_integration \
		-- --ignored --nocapture reaper_marks_stale --test-threads=1 \
	|| { echo "❌ test-saas failed (Rust)"; exit 1; }; \
	echo "Rust SaaS E2E passed"; \
		API_PORT=$${ASTRA_API_PORT:-$(DEFAULT_API_PORT)}; \
	HEALTH=$$(curl -sf -o /dev/null -w '%{http_code}' "http://127.0.0.1:$$API_PORT/health" 2>/dev/null || echo 000); \
	if [ "$$HEALTH" = "200" ]; then \
		if command -v npm >/dev/null 2>&1; then \
			echo "Running @astra/sdk SaaS remote (http://127.0.0.1:$$API_PORT)..."; \
			cd packages/sdk && npm ci --no-audit --no-fund --ignore-scripts && \
			ASTRA_SDK_BASE_URL="http://127.0.0.1:$$API_PORT" npm run test:integration:saas \
			|| { echo "❌ test-saas failed (SDK)"; exit 1; }; \
		else \
			echo "Skipping @astra/sdk SaaS remote (npm not in PATH; install Node or run: cd packages/sdk && npm run test:integration:saas)"; \
		fi; \
	else \
		echo "Skipping @astra/sdk SaaS remote (astra-server not healthy on :$$API_PORT)"; \
	fi; \
	echo "✅ SaaS platform E2E passed"

# SaaS E2E line coverage (llvm-cov): same Rust tests as test-saas, serial execution.
# Reports: coverage/saas-llvm/summary.txt (+ file-lines.txt, html/)
# Tip: run `make dev-stop` first if dev-api holds DB connections (pool cap errors).
SAAS_COV_DIR := coverage/saas-llvm
.PHONY: test-saas-coverage
test-saas-coverage:
	@command -v cargo-llvm-cov >/dev/null 2>&1 || { \
		echo "cargo-llvm-cov not found; install with: cargo install cargo-llvm-cov"; exit 1; \
	}
	@rustup component add llvm-tools-preview >/dev/null 2>&1 || { \
		echo "llvm-tools-preview required; run: rustup component add llvm-tools-preview"; exit 1; \
	}
	@if [ ! -f .env ]; then \
		echo "No .env found — creating from .env.example..."; \
		cp .env.example .env; \
	fi
	@set -a; [ -f .env ] && . ./.env; set +a; \
	TEST_DB=$${ASTRA_TEST_DATABASE:-astra_runtime_test}; \
	mkdir -p $(SAAS_COV_DIR); \
	echo "Running SaaS E2E with llvm coverage (database=$$TEST_DB, --test-threads=1)..."; \
	echo "NOTE: if tests fail with connection cap, run: make dev-stop"; \
	ASTRA_DATABASE=$$TEST_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
	ASTRA_TEST_DB_IT=1 ASTRA_TEST_E2E_SECRET=$${ASTRA_TEST_E2E_SECRET:-system-matrix-e2e-secret} \
	ASTRA_BACKEND_SERVICE_KEY=$${ASTRA_BACKEND_SERVICE_KEY:-test-service-key-e2e} \
	ASTRA_LLM_RETRY_BASE_MS=$${ASTRA_LLM_RETRY_BASE_MS:-10} \
	ASTRA_DEFAULT_RETRY_AFTER_MS=$${ASTRA_DEFAULT_RETRY_AFTER_MS:-10} \
	ASTRA_BCRYPT_COST=$${ASTRA_BCRYPT_COST:-4} \
	RUST_MIN_STACK=$${RUST_MIN_STACK:-16777216} \
	ASTRA_DB_POOL_MAX_CONNECTIONS=$${ASTRA_DB_POOL_MAX_CONNECTIONS:-5} \
	ASTRA_DB_GLOBAL_MAX_CONNECTIONS=$${ASTRA_DB_GLOBAL_MAX_CONNECTIONS:-10000} \
	CARGO_INCREMENTAL=0 cargo llvm-cov test $(CARGO_MANIFEST_FLAG) \
		--no-report --ignore-run-fail \
		-p astra-runtime \
		--features e2e-hooks \
		--test system_matrix_http_e2e \
		-- --ignored e2e_matrix_saas_ --test-threads=1; \
	RUNTIME_EXIT=$$?; \
	ASTRA_DATABASE=$$TEST_DB ASTRA_DATABASE_PREFIX="" ASTRA_AUTO_CREATE_DATABASE=1 \
	ASTRA_TEST_DB_IT=1 \
	ASTRA_DB_POOL_MAX_CONNECTIONS=$${ASTRA_DB_POOL_MAX_CONNECTIONS:-5} \
	ASTRA_DB_GLOBAL_MAX_CONNECTIONS=$${ASTRA_DB_GLOBAL_MAX_CONNECTIONS:-10000} \
	CARGO_INCREMENTAL=0 cargo llvm-cov test $(CARGO_MANIFEST_FLAG) \
		--no-report --ignore-run-fail \
		-p astra-services \
		--test session_reaper_db_integration \
		-- --ignored reaper_marks_stale; \
	SERVICES_EXIT=$$?; \
	if [ $$RUNTIME_EXIT -ne 0 ] || [ $$SERVICES_EXIT -ne 0 ]; then \
		echo "⚠️  Some SaaS tests failed; generating coverage report anyway (--ignore-run-fail)"; \
	fi; \
	echo "Generating coverage reports -> $(SAAS_COV_DIR)/"; \
	cargo llvm-cov report $(CARGO_MANIFEST_FLAG) \
		--summary-only \
		-p astra-runtime -p astra-services \
		| tee $(SAAS_COV_DIR)/summary.txt; \
	cargo llvm-cov report $(CARGO_MANIFEST_FLAG) \
		--text \
		-p astra-runtime -p astra-services \
		-show-instantiations=false \
		-show-regions=false \
		| tee $(SAAS_COV_DIR)/file-lines.txt; \
	cargo llvm-cov report $(CARGO_MANIFEST_FLAG) \
		--html \
		-p astra-runtime -p astra-services \
		--output-dir $(SAAS_COV_DIR)/html; \
	echo "✅ Line coverage report: $(SAAS_COV_DIR)/summary.txt (+ html/)"

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
	@echo "Running @astra/sdk offline (typecheck, Vitest with coverage + Mode A E2E, build)..."
	@cd packages/sdk && npm ci --no-audit --no-fund --ignore-scripts
	@cd packages/sdk && npm run typecheck
	@cd packages/sdk && ASTRA_SDK_E2E=1 npm run test:coverage
	@cd packages/sdk && npm run build

.PHONY: test-web-offline
test-web-offline: test-sdk-offline
	@echo "Running astra-web offline (typecheck, Vitest, build)..."
	@cd web && npm ci
	@cd web && npm run ci

# @astra/sdk — Vitest Mode B (ASTRA_SDK_BASE_URL) + sdk-online-smoke; requires astra-server (e.g. make dev-start)
.PHONY: test-sdk-online
test-sdk-online:
	@echo "Running @astra/sdk online (Vitest integration + test:online) — ensure API is up (e.g. make dev-start)..."
	@cd packages/sdk && npm ci --no-audit --no-fund --ignore-scripts
	@bash -ec 'set -a; [ -f "$(CURDIR)/.env" ] && . "$(CURDIR)/.env"; set +a; \
		export ASTRA_SDK_E2E=1; \
		export ASTRA_SDK_BASE_URL="$${ASTRA_SDK_BASE_URL:-http://127.0.0.1:$${ASTRA_API_PORT:-$(DEFAULT_API_PORT)}}"; \
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
# Runs the YAML cases at crates/astra-test-harness/cases against
# a fallback model list. Requires a running API server + fresh login.
#
# Variables:
#   MODELS       — fallback model list (default: qwen-flash)
#   CASES        — path to suite directory
#   FILTER       — glob pattern to select cases (e.g. "fork_*")
#   FORCE_MODEL  — override all case-level models with this one
#   PARALLEL     — concurrency (default: 1)
#   RUNS         — repeat each (case, model) pair N times (default: 1)
#   JUDGER       — judger model (default: same as MODELS)
#   SKIP_JUDGER  — set to 1 to skip the LLM judger step
#   PROFILE      — astra credential profile name
#   ARTIFACTS    — directory to persist per-case artifacts
#   FORMAT       — output format: text or json (default: text)
#
# Examples:
#   make test-harness MODELS=qwen-flash
#   make test-harness FILTER="fork_*" PARALLEL=4 RUNS=3
#   make test-harness MODELS=claude-sonnet-4-6 JUDGER=qwen-flash
# ----------------------------------------------------------------------------
.PHONY: test-harness
test-harness:
	@echo "Running astra-test-harness (unit tests + live suite)..."
	@$(CARGO) test $(CARGO_MANIFEST_FLAG) -p astra-test-harness
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-test-harness --release
	@MODELS="$${MODELS:-qwen-flash}"; \
	CASES="$${CASES:-crates/astra-test-harness/cases}"; \
	FILTER="$${FILTER:-}"; \
	FORCE_MODEL="$${FORCE_MODEL:-}"; \
	PARALLEL="$${PARALLEL:-1}"; \
	RUNS="$${RUNS:-1}"; \
	JUDGER="$${JUDGER:-$$MODELS}"; \
	SKIP_JUDGER="$${SKIP_JUDGER:-}"; \
	PROFILE="$${PROFILE:-}"; \
	ARTIFACTS="$${ARTIFACTS:-}"; \
	FORMAT="$${FORMAT:-text}"; \
	echo "  cases=$$CASES models=$$MODELS judger=$$JUDGER parallel=$$PARALLEL runs=$$RUNS"; \
	ARGS="--suite $$CASES --models $$MODELS --format $$FORMAT"; \
	[ -n "$$FILTER" ] && ARGS="$$ARGS --filter $$FILTER"; \
	[ -n "$$FORCE_MODEL" ] && ARGS="$$ARGS --force-model $$FORCE_MODEL"; \
	ARGS="$$ARGS --parallel $$PARALLEL --runs $$RUNS"; \
	[ -n "$$PROFILE" ] && ARGS="$$ARGS --profile $$PROFILE"; \
	[ -n "$$ARTIFACTS" ] && ARGS="$$ARGS --artifacts-dir $$ARTIFACTS"; \
	if [ -n "$$SKIP_JUDGER" ]; then \
		ARGS="$$ARGS --no-judger"; \
	else \
		ARGS="$$ARGS --judger-model $$JUDGER"; \
	fi; \
	[ -n "$${SUMMARIZE:-}" ] && ARGS="$$ARGS --summarize"; \
	[ -n "$${SUMMARIZE_MODEL:-}" ] && ARGS="$$ARGS --summarize-model $${SUMMARIZE_MODEL}"; \
	./target/release/astra-test $$ARGS

.PHONY: test-harness-capabilities
test-harness-capabilities: validate-capability-matrix ## Audit typed anchors, then run model capability probes and prompt variants with DeepSeek Flash
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-test-harness --release
	@$(CARGO) build $(CARGO_MANIFEST_FLAG) -p astra-cli --release --bin astra
	@mkdir -p target/astra-test-harness/capabilities
	@./target/release/astra-test --suite crates/astra-test-harness/cases --audit-capabilities
	@./target/release/astra-test --suite crates/astra-test-harness/cases \
		--capability-probes --force-model deepseek-v4-flash \
		--prompt-variants --judger-model deepseek-v4-flash \
		--artifacts-dir target/astra-test-harness/capabilities/artifacts \
		--report-file target/astra-test-harness/capabilities/report.json \
		--eval-file target/astra-test-harness/capabilities/eval.json \
		--parallel "$${PARALLEL:-1}" --runs "$${RUNS:-1}"

# ============================================================================
# Code Quality
# ============================================================================

.PHONY: check
check: lint format-check type-check check-web
	@echo "✅ All static checks passed!"

.PHONY: ci
ci: check test
	@echo "✅ All CI checks passed!"

.PHONY: toolchain-check
toolchain-check:
	@toolchain_version=$$(sed -nE 's/^channel = "([^"]+)"/\1/p' rust-toolchain.toml); \
	docker_version=$$(sed -nE 's/^ARG RUST_VERSION=([0-9.]+)-.*/\1/p' Dockerfile); \
	if [ -z "$$toolchain_version" ] || [ -z "$$docker_version" ] || [ "$$toolchain_version" != "$$docker_version" ]; then \
		echo "❌ Rust toolchain mismatch: rust-toolchain.toml=$${toolchain_version:-missing}, Dockerfile=$${docker_version:-missing}"; \
		exit 1; \
	fi

.PHONY: lint
lint: toolchain-check sweep
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

# RustSec dependency audit (same gate as GitHub static-checks workflow).
.PHONY: audit
audit:
	@command -v cargo-audit >/dev/null 2>&1 || { echo "cargo-audit not found; install with: cargo install cargo-audit"; exit 1; }
	@cargo audit --no-fetch

.PHONY: type-check
type-check: sweep
	@echo "Running compile checks..."
	@$(CARGO) check $(CARGO_MANIFEST_FLAG) --all-targets

.PHONY: check-web
check-web:
	@echo "Checking astra-web typecheck..."
	@cd packages/sdk && npm ci --ignore-scripts
	@cd packages/sdk && npm run build
	@cd web && npm ci
	@cd web && npm run typecheck

# ============================================================================
# Memoria (Memory Service)
# ============================================================================

.PHONY: memoria-start
memoria-start: dev-deps-up
	@echo "Memoria API: http://localhost:8100  Swagger: http://localhost:8100/docs"

.PHONY: memoria-stop
memoria-stop:
	@$(DEPS_COMPOSE) stop memoria

.PHONY: memoria-logs
memoria-logs:
	@$(DEPS_COMPOSE) logs -f memoria

.PHONY: memoria-status
memoria-status:
	@$(DEPS_COMPOSE) ps memoria

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
		SQL="DROP DATABASE IF EXISTS $$DB_NAME; CREATE DATABASE $$DB_NAME;"; \
		scripts/dev/mysql-client.sh -e "$$SQL"; \
		echo "✅ Database reset complete"; \
	else \
		echo "Cancelled"; \
	fi

.PHONY: test-mysql-client
test-mysql-client:
	@bash scripts/dev/test-mysql-client.sh
