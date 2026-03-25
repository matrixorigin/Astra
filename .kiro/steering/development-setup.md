---
inclusion: always
---

# Development Environment Setup

## Prerequisites

- Rust toolchain
- Docker
- Make
- Git

Repo development is Rust-first. Do **not** set up a Python virtualenv just to work on the server.
The only Python-specific path that still exists is the published CLI installer in `scripts/install.sh`.

## Quick Start

```bash
git clone <repo-url>
cd mo-dev-agent
make dev-init
make dev-start
make dev-status
```

Open `http://localhost:8000/docs` when the API is running.

## Daily Workflow

```bash
make dev-start
make dev-api-restart
make dev-status
make dev-stop
```

## Validation Workflow

```bash
make format-check
make type-check
make lint
make test
make test-integration
```

Use direct cargo commands when you need a narrower loop:

```bash
cargo test --manifest-path rust/Cargo.toml -p mo-agent-runtime --test http_contract
cargo check --manifest-path rust/Cargo.toml
```

## Troubleshooting

- If `cargo` is missing, install the Rust toolchain first.
- If services fail to start, check Docker and `make dev-status`.
- If a change touches API-shell behavior, prefer contract tests in `rust/crates/api-shell/tests/`.
