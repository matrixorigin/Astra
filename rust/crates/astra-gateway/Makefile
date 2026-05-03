# Astra Gateway — standalone Makefile
# Usage: cd rust/crates/astra-gateway && make <target>

CARGO ?= cargo
RELEASE_BIN = ../../target/release/astra-gateway
ASTRA_BIN = ../../target/release/astra
CONFIG ?= gateway.yaml

.PHONY: help build run login-weixin setup test test-live test-offline lint clean docker

help:
	@echo "Astra Gateway"
	@echo ""
	@echo "  make build         - Build release binary"
	@echo "  make run           - Build + run (uses gateway.yaml)"
	@echo "  make login-weixin  - QR scan to get WeChat credentials"
	@echo "  make setup         - Interactive setup wizard"
	@echo "  make test          - Run unit tests"
	@echo "  make test-live     - Run live e2e tests (requires LLM + DB)"
	@echo "  make test-offline  - Run offline fixture tests"
	@echo "  make lint          - Clippy + format check"
	@echo "  make docker        - Build Docker image"
	@echo "  make clean         - Remove build artifacts"

build:
	@echo "Building astra-gateway + astra CLI (release)..."
	@$(CARGO) build --manifest-path ../../Cargo.toml -p astra-gateway -p astra-cli --release
	@echo "Binary: $(RELEASE_BIN)"

run: build
	@if [ ! -f $(CONFIG) ]; then \
		echo "No $(CONFIG) found — copying from template..."; \
		cp gateway.example.yaml $(CONFIG); \
		echo "Created $(CONFIG). Edit it or run: make login-weixin"; \
	fi
	@$(RELEASE_BIN) --config $(CONFIG)

login-weixin: build
	@$(RELEASE_BIN) --config $(CONFIG) login-weixin

setup:
	@bash setup.sh

test:
	@$(CARGO) test --manifest-path ../../Cargo.toml -p astra-gateway --lib

test-live:
	@no_proxy=127.0.0.1,localhost $(CARGO) test --manifest-path ../../Cargo.toml -p astra-gateway --test e2e_cli_bridge -- --ignored

test-offline:
	@$(CARGO) test --manifest-path ../../Cargo.toml -p astra-gateway --test offline_cli_bridge

lint:
	@$(CARGO) clippy --manifest-path ../../Cargo.toml -p astra-gateway -- -D warnings
	@$(CARGO) fmt --manifest-path ../../Cargo.toml -p astra-gateway -- --check

docker:
	@cd ../../../ && docker build -f rust/crates/astra-gateway/Dockerfile -t astra-gateway .

clean:
	@$(CARGO) clean --manifest-path ../../Cargo.toml -p astra-gateway
