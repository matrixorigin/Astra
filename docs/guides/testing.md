# Testing Guide

This repository is validated through Rust-first checks and contract tests.

## Primary Commands

```bash
# Full Rust workspace
make test

# API-shell integration contracts
make test-integration
make test-api

# Static validation
make check
make format-check
make lint
make type-check
```

For direct cargo usage:

```bash
cargo test --manifest-path rust/Cargo.toml -q
cargo check --manifest-path rust/Cargo.toml
```

## Where Tests Live

- `rust/crates/api-shell/tests/` - HTTP, auth, session, bridge, routing, persistence, and contract tests
- `tests/fixtures/` - shared fixture data used by validation flows

## Recommended Workflow

```bash
# 1. Run the smallest relevant contract test while iterating
cargo test --manifest-path rust/Cargo.toml -p mo-agent-runtime --test auth_contract

# 2. Expand to the API-shell contract suite
make test-integration

# 3. Finish with full workspace + static checks
make check
make test
```

## What "done" Means

A change is not complete until:

- formatting passes
- compile/type checks pass
- clippy passes
- the relevant Rust tests pass

## Notes

