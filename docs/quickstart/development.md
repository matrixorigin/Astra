# Developer Setup

## Prerequisites

- Git, Make, and OpenSSL
- Rust via `rustup` (the repository pins the toolchain)
- Node.js 24 (pinned in `.nvmrc`)
- Docker with Docker Compose
- `mysql` client (optional, for `make dev-db-connect`)

## First-Time Setup

```bash
git clone https://github.com/matrixorigin/Astra.git
cd Astra
cp .models.yaml.example .models.yaml
make dev-init
```

`dev-init` creates `.env`, generates local secrets, installs pinned JavaScript
dependencies, and fetches Rust dependencies. Configure a real embedding
endpoint in `.env`, or explicitly select mock embeddings for local tests. The
default start is Server-only and uses a debug server build for a fast edit
loop.

```bash
make build-cli-debug
make dev-start
make dev-status
```

On a fresh database, establish identity and load a model before connecting the
User Runner:

```bash
./target/debug/astra admin register
./target/debug/astra admin model load .models.yaml --update-existing
```

Keep `.models.yaml` and `.env` credentials out of Git.

## Project Structure

```
crates/
  core/          kernel contracts and shared domain types
  services/      sessions, journals, durable tasks, and storage
  runtime/       HTTP server, handlers, orchestration, and contracts
  astra-cli/     CLI, TUI, administration, and local agent surface
  astra-edge/    User Runner and private capability boundary
  astra-pipeline/ Context Pipeline implementation
packages/sdk/    TypeScript SDK
web/             Web dashboard
```

## Development Workflow

```bash
# Deterministic daily startup: dependencies + debug API + Web
make dev-start

# Code → test → commit cycle
make dev-api-restart-debug
make test-contract        # focused API/config contracts
make test-offline         # broader offline gate
make check                # format, type, and lint checks

# Add host capabilities only when the scenario requires them
make dev-start-server-edge

# Shutdown
make dev-stop
```

Use a narrower package test while iterating whenever one owns the change.
`make test` includes online lanes and requires live dependencies; it is not the
default inner-loop command. See the [testing guide](../guides/testing.md) for
the lane selected by each boundary.

## Testing

```bash
make test-offline         # offline Rust, SDK, Web, hooks, and profile tests
make test                 # test-offline + test-online (DB required)
make test-online          # only #[ignore] Matrix E2E + multi-agent integration
make test-contract        # contract tests (http/admin/config)
make lint                 # cargo clippy --workspace -- -D warnings
make format-check         # formatting check
```

Contract tests live in `crates/runtime/tests/`; fixtures live in
`fixtures/contracts/`.

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

- Schema auto-created on API startup when `ASTRA_AUTO_CREATE_DATABASE=1`
- DDL lives in `crates/services/src/storage.rs`
- All tables use `IF NOT EXISTS` — safe to re-run

## Useful Make Targets

```bash
make help                 # full list
make dev-status           # check all service status
make dev-start-server-only # reset to no local User Runner
make dev-start-server-edge # start and connect this checkout's User Runner
make dev-deps-status      # check MatrixOne + Memoria
make dev-deps-logs        # tail dependency logs
make dev-api-logs         # tail API server log
make dev-deps-clean       # stop + delete all data (destructive)
```
