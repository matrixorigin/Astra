# mo-agent-engine

Rust-first agent platform for auditable chat runs, session history, replay, skills, admin operations, and MatrixOne-backed state.

## Highlights

- Rust API surface in `rust/crates/api-shell`
- Auth, sessions, chat runs, replay, admin, models, skills, and workflow endpoints
- SSE/chat-turn bridge plumbing with persisted turn events and side effects
- MatrixOne + Redis integration for durable state and caching
- Contract-heavy Rust test suite with `cargo`, `clippy`, and formatting checks

## Quick Start

### Development

Prerequisites:

- Rust toolchain
- Docker
- Make
- Git

```bash
# Initialize local config and fetch Rust dependencies
make dev-init

# Start dependencies and the API server
make dev-start

# Check status
make dev-status
```

Open `http://localhost:8000/docs` after startup.

> Repo development does **not** require creating a Python virtual environment.
> The only remaining Python requirement is `scripts/install.sh`, which is a published CLI installer path rather than the repo's development workflow.

### Docker

```bash
cp .env.example .env
make dev-start-docker
make dev-status
```

## Daily Commands

```bash
make dev-start
make dev-api-restart
make dev-stop
make dev-status
```

## Testing and Validation

```bash
# Full Rust workspace tests
make test

# API-shell integration contract suite
make test-integration
make test-api

# Static checks
make check
make format-check
make lint
make type-check

# Direct cargo invocation when needed
cargo test --manifest-path rust/Cargo.toml -q
```

The Rust contract tests live under `rust/crates/api-shell/tests/`.

## CLI Examples

### `mo-agent`

```bash
mo-agent login
mo-agent chat -m "帮我分析这个仓库"
mo-agent session list --limit 20
mo-agent health
```

### `mo-admin`

```bash
mo-admin login
mo-admin init
mo-admin model list
mo-admin audit --limit 20
```

## Repository Layout

```text
mo-dev-agent/
├── rust/crates/api-shell/    # Rust HTTP/API crate and contract tests
├── deployment/               # Docker and deployment assets
├── scripts/                  # Dev, setup, install, and ops scripts
├── skills/                   # Skill definitions and examples
├── tests/fixtures/           # Shared test fixtures
└── docs/                     # User-facing documentation
```

## Documentation

- `docs/README.md`
- `docs/guides/testing.md`
- `deployment/README.md`
- `docs/reference/makefile-commands.md`

## Status

The Rust API shell is the primary implementation. Remaining Python references are limited to explicit compatibility or packaging paths, not the server runtime.
