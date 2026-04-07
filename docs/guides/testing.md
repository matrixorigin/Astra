# Testing Guide

This repository is validated through Rust-first checks, contract tests (fast, many with stub services), and optional **live MatrixOne** system E2E.

## Primary Commands

```bash
# Full Rust workspace (includes bridge-e2e-hooks for `astra-runtime`; does not run ignored live-DB E2E unless env is set)
make test

# Same as make test plus ignored Matrix E2E + astra-services multi-agent integration
make test-integration

# Narrow contract smoke (HTTP, admin, config — real routing, stub deps where the test builds AppState)
make test-contract

# Static validation
make check
make format-check
make lint
make type-check
```

Direct `cargo` usage:

```bash
cargo test --manifest-path rust/Cargo.toml -q
cargo check --manifest-path rust/Cargo.toml
```

## Where Tests Live

- `rust/crates/runtime/tests/` — HTTP integration tests for `astra-runtime` (including `*_contract.rs`, `system_matrix_http_e2e/`, bridge E2E).
- `rust/crates/services/tests/` — service-layer tests (e.g. `multi_agent_integration` with live DB when `ASTRA_MULTI_AGENT_IT=1`).
- `fixtures/contracts/` — JSON fixtures for contract tests that load shared request/response shapes.
- `tests/fixtures/golden_sessions/` — golden session payloads for selected flows.
- Capability ↔ route ↔ E2E mapping: [`docs/testing/system-e2e-matrix.md`](../testing/system-e2e-matrix.md).
- Coverage matrix (what replaced stub tests, large-binary audit): [`docs/testing/coverage-matrix.md`](../testing/coverage-matrix.md).

## Live MatrixOne system E2E

```bash
ASTRA_SYSTEM_MATRIX_E2E=1 \
ASTRA_BRIDGE_TEST_SECRET=system-matrix-e2e-secret \
cargo test -p astra-runtime --test system_matrix_http_e2e --features bridge-e2e-hooks -- \
  --ignored --nocapture
```

Requires the same environment as `astra-server`: `MATRIXONE_*`, `JWT_SECRET_KEY` / `SECRET_KEY`, `TOKEN_ENCRYPTION_KEY`, Redis, embedding settings per `astra_core::AppSettings::from_env`. Use a local `.env` if you use one for development.

## Recommended Workflow

```bash
# 1. Smallest relevant target while iterating
cargo test --manifest-path rust/Cargo.toml -p astra-runtime --test http_contract

# 2. Core HTTP contract smoke
make test-contract

# 3. Full workspace + bridge hooks
make test

# 4. With MatrixOne + Redis up: full ignored E2E
make test-integration
```

## What "done" Means

A change is not complete until:

- formatting passes
- compile/type checks pass
- clippy passes
- the relevant Rust tests pass (including PR Matrix E2E when touching server/persistence paths)
