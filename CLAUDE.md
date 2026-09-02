# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

A second file at `.claude/CLAUDE.md` is also loaded — it carries the MANDATORY development rules (error handling, persisted-state tracing, SQL/vector query discipline, test isolation). Read it before changing service or DB code.

## Project Overview

`astra-engine` — a Rust-first agent platform for auditable chat runs, session history, replay, skills, admin operations, and MatrixOne-backed state. The flagship binary is `astra-server` (Axum HTTP API); the operator-facing surfaces are the `astra` CLI (interactive TUI + scripting + admin operations), and a Next.js admin dashboard under `web/`.

## Cargo Workspace Lives At Repo Root

The Rust workspace `Cargo.toml` is at the repository root. Run raw `cargo`
commands from the repo root, or pass `--manifest-path Cargo.toml` when invoking
Cargo from another directory:

```bash
# ✓ repo root
cargo build -p astra-runtime

# ✓ any directory
cargo build --manifest-path Cargo.toml -p astra-runtime
```

Prefer `make <target>` from the repo root for common workflows.

## Build & Test

```bash
make build              # Release workspace build (sweeps stale artifacts first)
make build-debug        # Debug, fast incremental
make build-cli          # astra CLI only
make build-server       # astra-server only

make test               # Full: offline + online (online needs MatrixOne running)
make test-offline       # Rust workspace (nextest) + e2e-hooks + @astra/sdk offline
make test-online        # Live MatrixOne #[ignore] suites + Matrix E2E
make test-live-llm      # Real provider APIs from .models.yaml (opt-in, ASTRA_LIVE_LLM=1)
make test-contract      # HTTP + admin contract binaries + astra-core settings JSON
make test-harness       # YAML declarative CLI cases (astra-test-harness)

make check              # lint + format-check + type-check (run before commit)
make lint               # cargo clippy --workspace -- -D warnings
make format             # rustfmt
make audit              # cargo-audit (needs: cargo install cargo-audit)
```

Run a single Rust test:

```bash
cargo nextest run -p astra-runtime --test http_contract
cargo nextest run -p astra-runtime some_test_name           # filter by name
cargo test --manifest-path Cargo.toml -p astra-runtime --test http_contract -- --nocapture
```

`cargo nextest` uses profiles in `.config/nextest.toml`. Default per-case slow-timeout is 30s (relaxed because of known contention in `session_sync_log` prune; see `plans/session-sync-log-prune-hotpath-*.md`). Override with `NEXTEST_OFFLINE_PROFILE=` / `NEXTEST_ONLINE_PROFILE=`.

Important env vars for tests:

- `ASTRA_TEST_DB_IT=1` — opt into online integration suites in `test-ignored-integration`
- `ASTRA_TEST_DB_IT_TEST_THREADS=1` — serialize online tests (`-j 1`)
- `ASTRA_TEST_DATABASE=astra_runtime_test` — separate DB for online tests
- `ASTRA_DATABASE_PREFIX=` — prefix the logical DB name (see `astra_core::resolve_database_name`); use it so local/CI never collides with production
- `ASTRA_SDK_ONLINE_E2E=1` — include @astra/sdk remote E2E in `test-online`
- `ASTRA_LIVE_LLM=1` — gate live-LLM token-usage suite

## Dev Loop

```bash
make dev-init           # First-time: generate JWT_SECRET_KEY, TOKEN_ENCRYPTION_KEY, CHAT_TURN_RUNTIME_ROOT_SECRET, fetch deps
make dev-start          # dev-deps-up → dev-deps-wait → dev-api-start  (release)
make dev-stop           # dev-api-stop + dev-deps-down
make dev-status         # all-service status
make dev-restart        # restart everything
make dev-clean          # destructive: stops + wipes deps data

make dev-deps-up        # MatrixOne :6001 + Memoria :8100 (docker compose)
make dev-deps-down
make dev-deps-clean     # destructive: wipes deployment/all-in-one/data
make dev-db-connect     # mysql CLI into MatrixOne

make dev-api-start          # release build of astra-server
make dev-api-start-debug    # debug build, fast iteration
make dev-api-restart        # stop + release start
make dev-api-restart-debug  # stop + debug start
make dev-api-logs

make dev-seed           # End-to-end bootstrap: recreate DB, restart API, register admin@mo.com / 11111111, load .models.yaml
```

After `make build`, binaries live at `target/release/` (or `debug/`): `astra-server`, `astra`.

## Logging & Observability

- `RUST_LOG=info` (or `astra_runtime=debug`, etc.) — standard tracing filter
- `ASTRA_LOG_FORMAT=json|pretty|compact` (default: json when stderr is not a TTY)
- `ASTRA_DIAGNOSTIC_LOG=1` — structured stderr logs from the `astra` CLI
- `ASTRA_LOG_FILE=/path/to.log` — JSON-line CLI logs to file (doesn't replace TUI output)
- HTTP handlers run inside a per-request span; access lines use target `astra.http.access` with `request_id` matching the `x-request-id` header. `/health` skips access logging.
- OpenTelemetry: build with `--features otel` on `astra-runtime` or `astra-edge`. Activate via `ASTRA_OTEL_ENABLED=1` or `OTEL_EXPORTER_OTLP_ENDPOINT`. Server flushes batches on SIGTERM/Ctrl+C; `kill -9` drops final spans.

## Workspace Architecture

Top-level layout:

```
Cargo.toml        # Cargo workspace root
crates/core/      astra-core: shared types, config, DB name resolution
crates/services/  sessions, journals, durable tasks, cloud sync
crates/runtime/   Axum HTTP server (astra-server bin), contract tests in tests/
crates/astra-cli/ astra TUI + scripting CLI, including admin subcommands
crates/astra-edge/   edge runtime (HTTP, WS, edge-cloud sync)
crates/astra-plan/   plan executor
crates/astra-skills/ skill loading + execution
crates/astra-tools/  built-in tools (server allowlist, executor allowlist, schemas)
crates/astra-turn-core/, astra-turn-types/  chat turn primitives
crates/astra-prompts/, astra-pipeline/, astra-sandbox/, astra-messaging/
crates/astra-config/, astra-credentials/, astra-logging/
crates/astra-test-harness/, astra-harness/  declarative test/run harness
crates/astra-thin-client/  stateless HTTP+SSE client for thin-client protocol
packages/sdk/     @astra/sdk (TypeScript) — Mode A in-process, Mode B remote E2E
web/              Next.js admin dashboard
.claude/skills/   Agent Skills for Claude-compatible agents
.agent/skills/    Agent Skills mirror for Agent-compatible runtimes
deployment/all-in-one/  docker-compose for MatrixOne + Memoria + API
docs/             quickstart/, guides/, reference/, design/, implementation/, testing/
fixtures/         contract JSON fixtures
plans/            in-flight workstreams (e.g. session-sync-log-prune-hotpath-*)
benchmarks/       perf scenarios
```

Stack:

- Edition 2024, clippy warnings = errors
- Async: Tokio • HTTP: Axum 0.8 • DB: SQLx against MatrixOne (MySQL protocol HTAP w/ vector, full-text, git4data, stage, pubsub, datalink)
- Errors: `thiserror` everywhere (no `anyhow` in library code)
- Memory service: Memoria (separate process at :8100, MCP-accessible)

## Skills

`.claude/skills/<name>/SKILL.md` are first-class workflows agents follow when invoked.
`.agent/skills/` carries the same curated set for Agent-compatible runtimes. Read the
SKILL.md _before_ starting the corresponding task — each enforces a focused workflow:

- `astra-dev` — Astra-specific engineering workflow, ownership map, targeted verification
- `review_changes` — context-aware code review with symbol-level impact analysis
- `review_code` — test-quality focus: unhappy paths, error scenarios, E2E with real DB assertions
- `verify_task` — build/test/lint checks → delivery report
- `analyze_session` — diagnose astra session issues (token waste, stalls, loops)
- `optimize_prompt` — reduce LLM context bloat
- `audit_cloud_sync` — edge↔cloud sync integrity
- `trace_delegation` — multi-agent delegation flows
- `unhappy_path_audit` — reachability-first failure-path audit

Project-local skills intentionally live under `.claude/skills/` and `.agent/skills/`.
The legacy root `skills/` tree is not used in this repository.

## Conventions (Beyond `.claude/CLAUDE.md`)

- **MatrixOne SQL discipline**: cosine index ⇒ cosine query (never L2); avoid JSON-column WHERE filters (full-table scan); vector/full-text tables prefer append + soft-delete over UPDATE/DELETE.
- **DB name resolution**: effective database = `${ASTRA_DATABASE_PREFIX}${ASTRA_DATABASE}` via `astra_core::resolve_database_name`. Don't hardcode names.
- **Capability-driven tool surface**: tool visibility is `surface_admits(scope, surface) ∧ caps.has_all(requires)` via `astra-turn-core::tool_surface`. New catalog tools declare `requires: &[Capability::...]`; do not add parallel tool-name allowlists.
- **Test isolation**: every test owns its IDs and cleans up; never depend on order or share state. E2E tests must verify DB state directly after mutation, not trust HTTP responses alone.
- **Per-case test budget**: 30s. If a single case approaches this, the contention is the bug — don't paper over with longer timeouts.
