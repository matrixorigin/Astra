# Development Guide

## Prerequisites

- Rust toolchain (edition 2024)
- Docker + Docker Compose
- Make
- `mysql` client (optional, for `make dev-db-connect`)

## First-Time Setup

See [README.md](../../README.md) Quick Start for initial configuration and startup.

## Project Structure

```
rust/crates/
  core/          shared types, config, error types
  services/      sessions, journals, durable tasks, storage DDL
  runtime/       Axum HTTP server, handlers, pipeline, contract tests
  astra-cli/     interactive CLI, slash commands, edge tools
  astra-admin/   admin CLI (register, model load, init, audit)
```

## Development Workflow

```bash
# Daily startup
make dev-deps-up          # MatrixOne + Memoria
make dev-api-start        # API server

# Code → test → commit cycle
make build                # release build
make test-offline         # unit + contract tests (no DB)
make test                 # full suite (DB required)
make check                # lint + format + type checks (run before commit)

# Restart after code changes
make dev-api-restart

# Shutdown
make dev-deps-down
```

## Testing

```bash
make test-offline         # workspace tests + bridge hooks (no DB)
make test                 # test-offline + test-online (DB required)
make test-online          # only #[ignore] Matrix E2E + multi-agent integration
make test-contract        # contract tests (http/admin/config)
make lint                 # cargo clippy --workspace -- -D warnings
make format-check         # formatting check
```

Contract tests live in `rust/crates/runtime/tests/`. Fixtures in `fixtures/contracts/`.

Each DB test must be fully isolated: unique IDs, no shared mutable state, no order dependency.

## Code Conventions

- `thiserror` for error types — never `anyhow` in library code
- Every error variant must carry enough context to locate the bug without a debugger
- Log at the boundary (handler, CLI entry), not deep in the call stack
- No `unsafe` without safety comments
- `CREATE TABLE IF NOT EXISTS` for all DDL — idempotent schema creation
- clippy warnings = errors

## Database

```bash
make dev-db-connect       # mysql CLI into MatrixOne
```

- Schema auto-created on API startup when `MATRIXONE_AUTO_CREATE_DATABASE=1`
- DDL lives in `rust/crates/services/src/storage.rs`
- All tables use `IF NOT EXISTS` — safe to re-run

## Useful Make Targets

```bash
make help                 # full list
make dev-status           # check all service status
make dev-deps-status      # check MatrixOne + Memoria
make dev-deps-logs        # tail dependency logs
make dev-api-logs         # tail API server log
make dev-deps-clean       # stop + delete all data (destructive)
```
