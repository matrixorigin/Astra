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
| `make test-integration` | Run API-shell integration contracts |
| `make test-api` | Run API-shell integration contracts |
| `make migration-contract-test` | Run selected HTTP/auth/admin/config contract tests |

## Static Checks

| Command | Description |
| --- | --- |
| `make check` | Run format, compile, and lint validation |
| `make format` | Run `cargo fmt` |
| `make format-check` | Check formatting |
| `make type-check` | Run `cargo check --all-targets` |
| `make lint` | Run `clippy` with warnings denied |
| `make lint-fix` | Apply formatter-driven cleanup |

## Build Outputs

| Command | Description |
| --- | --- |
| `make rust-build` | Build the Rust workspace in debug mode (`rust/target/debug`) |
| `make rust-build-release` | Build the Rust workspace in release mode (`rust/target/release`) |
| `make cli-build` | Build `mo-agent`, `mo-admin`, and `mo-agent-server` in debug mode |
| `make cli-build-release` | Build `mo-agent`, `mo-admin`, and `mo-agent-server` in release mode |
| `make print-bin-paths` | Print the exact debug/release binary paths |

## Useful Direct Cargo Commands

```bash
cargo test --manifest-path rust/Cargo.toml -q
cargo check --manifest-path rust/Cargo.toml
cargo fmt --all --manifest-path rust/Cargo.toml
```
