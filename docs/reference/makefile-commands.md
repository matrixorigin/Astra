# Makefile Commands Reference

## Core Development

| Command | Description |
| --- | --- |
| `make dev-init` | Create `.env`, generate local secrets, and fetch development dependencies |
| `make dev-start` | Start dependencies and the API from source |
| `make dev-start-server-only` | Reset to dependencies + debug API + Web without a repo-launched User Runner |
| `make dev-start-server-edge` | Start Server-only and connect this checkout as a User Runner |
| `make dev-stop` | Stop local services |
| `make dev-status` | Show dependency and API status |
| `make dev-api-restart` | Restart the source-mode API server |
| `make dev-api-restart-debug` | Rebuild and restart the source-mode API server in debug mode |
| `make dev-start-docker` | Start the app stack in Docker mode |

## Testing

| Command | Description |
| --- | --- |
| `make test` | `test-offline` + `test-online` (see rows below; SDK **remote** E2E only if `ASTRA_SDK_ONLINE_E2E=1`) |
| `make test-offline` | Rust: workspace + `e2e-hooks`. JS SDK (`packages/sdk`): `typecheck`, `ASTRA_SDK_E2E=1` Vitest with coverage (unit + in-process Mode A), `build` |
| `make test-online` | Rust: astra-runtime `#[ignore]` + Matrix / services ignored suites (needs **live DB** e.g. `dev-deps` in CI). **JS SDK** remote Vitest/smoke runs **only** when `ASTRA_SDK_ONLINE_E2E=1` (and a live API, e.g. `make dev-start`); otherwise skipped so CI does not need HTTP on :17001 |
| `make test-sdk-offline` | `@astra/sdk` only — same SDK steps as the SDK portion of `test-offline` |
| `make test-sdk-online` | `@astra/sdk` only — Vitest remote integration + `test:online` smoke; **requires a running API**; also invoked from `test-online` when `ASTRA_SDK_ONLINE_E2E=1` |
| `make test-contract` | Run `http_contract` / `admin_contract` (astra-runtime) + settings JSON contract (`astra-core` `settings_contract_tests`) |

## Static Checks

| Command | Description |
| --- | --- |
| `make check` | Run format, compile, and lint validation |
| `make ci` | Run all checks + tests |
| `make format` | Run `cargo fmt` |
| `make format-check` | Check formatting |
| `make type-check` | Run `cargo check --all-targets` |
| `make lint` | Run `clippy` with warnings denied |
| `make lint-fix` | Apply formatter-driven cleanup |

## Build

| Command | Description |
| --- | --- |
| `make build` | Build the Rust workspace in release mode (same as `build-release`) |
| `make build-release` | Build the Rust workspace in release mode (`target/release`) |
| `make build-server` | Build `astra-server` in release mode (same as `build-server-release`) |
| `make build-server-release` | Build `astra-server` in release mode |
| `make build-cli` | Build the `astra` CLI in release mode (same as `build-cli-release`) |
| `make build-cli-release` | Build the `astra` CLI in release mode |

## Published Compose Stack

| Command | Description |
| --- | --- |
| `make stack-env` | Create deployment environment files and generate local secrets |
| `make stack-start` | Start the published all-in-one stack, wait for health, and print the next CLI steps |
| `make stack-up` | Start or resume the configured stack without running the guided verification |
| `make stack-verify` | Check stack health and run a memory round trip |
| `make stack-down` | Stop the stack while preserving its data |
| `make stack-clean` | Immediately delete the stack and its persisted data (destructive; no confirmation prompt) |

## Memoria

| Command | Description |
| --- | --- |
| `make memoria-start` | Start Memoria memory service |
| `make memoria-stop` | Stop Memoria |
| `make memoria-logs` | Tail Memoria logs |
| `make memoria-status` | Show Memoria status |

Memoria persists through the shared MatrixOne dependency. Use
`make dev-deps-clean` when you intentionally need to remove all local
dependency data; that command prompts before deletion.

## All-in-One first run

| Command | Description |
| --- | --- |
| `make stack-setup` | State-aware wizard for embedding preflight, data-preserving stack reconciliation, runtime verification, admin, and model probe |
| `make stack-start` | Non-interactively initialize configuration, start the stack, and verify health plus a memory round trip |
| `make stack-env` | Create local `.env` and generate secrets without prompting |
| `make stack-up` | Start the configured Compose stack |
| `make stack-up STACK_RECREATE=1` | Recreate containers and network attachments while preserving volumes |
| `make stack-verify` | Verify health and a memory round trip |

## Useful Direct Cargo Commands

```bash
cargo test --manifest-path Cargo.toml -q
cargo check --manifest-path Cargo.toml
cargo fmt --all --manifest-path Cargo.toml
```
