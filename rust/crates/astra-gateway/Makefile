# Astra Gateway — standalone Makefile
# Usage: cd rust/crates/astra-gateway && make <target>

# Load .env into shell commands (if present)
DOT_ENV := $(shell [ -f .env ] && echo .env)
ifdef DOT_ENV
  include .env
  export
endif

CARGO ?= cargo
RELEASE_BIN = ../../target/release/astra-gateway
CONFIG ?= gateway.yaml
PID_FILE = /tmp/astra-gateway.pid
LOG_FILE = /tmp/astra-gateway.log

.PHONY: help build run start stop restart status logs login-weixin setup test test-live test-offline lint clean docker

help:
	@echo "Astra Gateway"
	@echo ""
	@echo "Lifecycle:"
	@echo "  make start         - Build + start in background (nohup)"
	@echo "  make stop          - Stop running gateway"
	@echo "  make restart       - Stop + start"
	@echo "  make status        - Show running status"
	@echo "  make logs          - Tail gateway logs"
	@echo "  make run           - Build + run in foreground (Ctrl+C to stop)"
	@echo ""
	@echo "Setup:"
	@echo "  make build         - Build release binary"
	@echo "  make login-weixin  - QR scan to get WeChat credentials"
	@echo "  make setup         - Interactive setup wizard"
	@echo ""
	@echo "Testing:"
	@echo "  make test          - Run unit tests"
	@echo "  make test-live     - Live e2e tests (requires LLM + DB)"
	@echo "  make test-offline  - Offline fixture tests"
	@echo "  make lint          - Clippy + format check"
	@echo ""
	@echo "Deploy:"
	@echo "  make docker        - Build Docker image"
	@echo "  make clean         - Remove build artifacts"

build:
	@echo "Building astra-gateway + astra CLI (release)..."
	@$(CARGO) build --manifest-path ../../Cargo.toml -p astra-gateway -p astra-cli --release
	@echo "Binary: $(RELEASE_BIN)"

# ── Foreground (dev) ──

run: build _ensure_config _ensure_astra_auth
	@$(RELEASE_BIN) --config $(CONFIG)

# ── Background (production) ──

start: build _ensure_config _ensure_astra_auth
	@if [ -f $(PID_FILE) ]; then \
		PID=$$(cat $(PID_FILE)); \
		if kill -0 $$PID 2>/dev/null && ps -p $$PID -o command= 2>/dev/null | grep -q 'astra-gateway'; then \
			echo "✅ Gateway already running (PID $$PID)"; \
		else \
			rm -f $(PID_FILE); \
			echo "Starting gateway in background..."; \
			nohup $(RELEASE_BIN) --config $(CONFIG) </dev/null > $(LOG_FILE) 2>&1 & \
			sleep 1; \
			if [ -f $(PID_FILE) ] && kill -0 $$(cat $(PID_FILE)) 2>/dev/null && ps -p $$(cat $(PID_FILE)) -o command= 2>/dev/null | grep -q 'astra-gateway'; then \
				echo "✅ Gateway started (PID $$(cat $(PID_FILE)))"; \
				echo "   Logs: $(LOG_FILE)"; \
			else \
				echo "❌ Gateway failed to start. Check $(LOG_FILE)"; \
				tail -10 $(LOG_FILE); \
				exit 1; \
			fi; \
		fi; \
	else \
		echo "Starting gateway in background..."; \
		nohup $(RELEASE_BIN) --config $(CONFIG) </dev/null > $(LOG_FILE) 2>&1 & \
		sleep 1; \
		if [ -f $(PID_FILE) ] && kill -0 $$(cat $(PID_FILE)) 2>/dev/null && ps -p $$(cat $(PID_FILE)) -o command= 2>/dev/null | grep -q 'astra-gateway'; then \
			echo "✅ Gateway started (PID $$(cat $(PID_FILE)))"; \
			echo "   Logs: $(LOG_FILE)"; \
		else \
			echo "❌ Gateway failed to start. Check $(LOG_FILE)"; \
			tail -10 $(LOG_FILE); \
			exit 1; \
		fi; \
	fi

stop:
	@if [ -f $(PID_FILE) ] && kill -0 $$(cat $(PID_FILE)) 2>/dev/null && ps -p $$(cat $(PID_FILE)) -o command= 2>/dev/null | grep -q 'astra-gateway'; then \
		echo "Stopping gateway (PID $$(cat $(PID_FILE)))..."; \
		kill $$(cat $(PID_FILE)); \
		sleep 1; \
		if kill -0 $$(cat $(PID_FILE)) 2>/dev/null; then \
			echo "Force killing..."; \
			kill -9 $$(cat $(PID_FILE)) 2>/dev/null; \
		fi; \
		rm -f $(PID_FILE); \
		echo "✅ Gateway stopped."; \
	else \
		echo "Gateway not running."; \
		rm -f $(PID_FILE); \
	fi

restart: stop start

status:
	@if [ -f $(PID_FILE) ] && kill -0 $$(cat $(PID_FILE)) 2>/dev/null && ps -p $$(cat $(PID_FILE)) -o command= 2>/dev/null | grep -q 'astra-gateway'; then \
		PID=$$(cat $(PID_FILE)); \
		UPTIME=$$(ps -o etime= -p $$PID 2>/dev/null | tr -d ' '); \
		MEM=$$(ps -o rss= -p $$PID 2>/dev/null | tr -d ' '); \
		echo "✅ Gateway running"; \
		echo "   PID: $$PID"; \
		echo "   Uptime: $$UPTIME"; \
		echo "   Memory: $$(( $${MEM:-0} / 1024 )) MB"; \
		echo "   Logs: $(LOG_FILE)"; \
		echo "   Config: $(CONFIG)"; \
	else \
		echo "❌ Gateway not running."; \
	fi

logs:
	@if [ -f $(LOG_FILE) ]; then \
		tail -f $(LOG_FILE); \
	else \
		echo "No log file found at $(LOG_FILE)"; \
	fi

# ── Setup ──

login-weixin: build _ensure_config
	@$(RELEASE_BIN) --config $(CONFIG) login-weixin

setup:
	@bash setup.sh

_ensure_config:
	@if [ ! -f $(CONFIG) ]; then \
		echo "No $(CONFIG) found — copying from template..."; \
		cp gateway.example.yaml $(CONFIG); \
		echo "Created $(CONFIG). Edit it or run: make login-weixin"; \
	fi

ASTRA_BIN = ../../target/release/astra

_ensure_astra_auth:
	@if [ -f $(ASTRA_BIN) ]; then \
		no_proxy=127.0.0.1,localhost $(ASTRA_BIN) whoami >/dev/null 2>&1 || { \
			echo "Astra CLI auth expired — refreshing..."; \
			USER=$${ASTRA_USERNAME:-gateway}; \
			PASS=$${ASTRA_PASSWORD:-gateway123}; \
			no_proxy=127.0.0.1,localhost $(ASTRA_BIN) login --username $$USER --password $$PASS >/dev/null 2>&1 || \
			no_proxy=127.0.0.1,localhost $(ASTRA_BIN) register --username $$USER --email $$USER@gateway.local --password $$PASS >/dev/null 2>&1 || \
			echo "⚠️  Astra auth failed — astra CLI may not work. Set ASTRA_USERNAME/ASTRA_PASSWORD or run: astra login"; \
		}; \
	fi

# ── Testing ──

test:
	@$(CARGO) test --manifest-path ../../Cargo.toml -p astra-gateway --lib

test-live:
	@no_proxy=127.0.0.1,localhost $(CARGO) test --manifest-path ../../Cargo.toml -p astra-gateway --test e2e_cli_bridge -- --ignored

test-offline:
	@$(CARGO) test --manifest-path ../../Cargo.toml -p astra-gateway --test offline_cli_bridge

lint:
	@$(CARGO) clippy --manifest-path ../../Cargo.toml -p astra-gateway -- -D warnings
	@$(CARGO) fmt --manifest-path ../../Cargo.toml -p astra-gateway -- --check

# ── Deploy ──

docker:
	@cd ../../../ && docker build -f rust/crates/astra-gateway/Dockerfile -t astra-gateway .

clean:
	@$(CARGO) clean --manifest-path ../../Cargo.toml -p astra-gateway
