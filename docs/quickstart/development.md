# Development Setup

## Prerequisites

- Rust toolchain
- Docker
- Make
- Git

You do not need a Python virtual environment for repo development.

## Setup

```bash
make dev-init
make dev-start
make dev-status
```

## Daily Loop

```bash
make dev-start
make dev-api-restart
make check
make test
make dev-stop
```

## Focused Validation

```bash
cargo test --manifest-path rust/Cargo.toml -p mo-agent-runtime --test http_contract
cargo check --manifest-path rust/Cargo.toml
```
