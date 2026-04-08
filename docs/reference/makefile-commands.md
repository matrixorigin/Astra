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
| `make build-release` | Build the Rust workspace in release mode (`rust/target/release`) |
| `make build-server` | Build `astra-server` in release mode (same as `build-server-release`) |
| `make build-server-release` | Build `astra-server` in release mode |
| `make build-cli` | Build `astra` and `astra-admin` in release mode (same as `build-cli-release`) |
| `make build-cli-release` | Build `astra` and `astra-admin` in release mode |

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
