# Makefile Commands Reference

## Core Development

| Command | Description |
| --- | --- |
| `make dev-init` | Create `.env` if needed and fetch Rust dependencies |
| `make dev-start` | Start dependencies and the API from source |
| `make dev-stop` | Stop local services |
| `make dev-status` | Show dependency and API status |
| `make dev-api-restart` | Restart the source-mode API server |
| `make dev-start-docker` | Start the app stack in Docker mode |

## Testing

| Command | Description |
| --- | --- |
| `make test` | Run Rust workspace tests |
| `make test-contract` | Run API-shell integration contracts |
| `make test-contract` | Run specific contract tests (http/admin/auth/config) |

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
| `make build` | Build the Rust workspace in debug mode (`rust/target/debug`) |
| `make build-release` | Build the Rust workspace in release mode (`rust/target/release`) |
| `make build-server` | Build `mo-agent-server` in debug mode |
| `make build-server-release` | Build `mo-agent-server` in release mode |
| `make build-cli` | Build `mo-agent` and `mo-admin` in debug mode |
| `make build-cli-release` | Build `mo-agent` and `mo-admin` in release mode |

## Memoria

| Command | Description |
| --- | --- |
| `make memoria-start` | Start Memoria memory service |
| `make memoria-stop` | Stop Memoria |
| `make memoria-logs` | Tail Memoria logs |
| `make memoria-status` | Show Memoria status |
| `make memoria-clean` | Stop and remove Memoria data |

## Useful Direct Cargo Commands

```bash
cargo test --manifest-path rust/Cargo.toml -q
cargo check --manifest-path rust/Cargo.toml
cargo fmt --all --manifest-path rust/Cargo.toml
```
