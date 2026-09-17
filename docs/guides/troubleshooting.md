# Troubleshooting Guide

## Quick Checks

```bash
make dev-status
make dev-api-logs
make dev-deps-logs
curl http://localhost:17001/health
```

## Cargo Missing

Install the Rust toolchain first, then rerun `make dev-init`.

## MatrixOne Not Ready

```bash
make dev-deps-down
make dev-deps-up
make dev-deps-wait
```

## API Not Starting

```bash
make dev-api-logs
make dev-api-restart
make type-check
```

## Test Failures After Refactor

```bash
make test-contract
make test
make check
```

## macOS Chat Reports No Session Execution Authority

If `astra chat` reports `this platform has no rename-resistant session execution
authority`, the installed client predates the macOS lease implementation. Check
`astra --version`, install the current patch release, and keep the deployment
checkout matched to that exact client version. Do not bypass the lease or reuse
a Linux binary built for a different platform.

## Stale Run Projection

If a run list or projection response looks stale while the durable run status is
correct, use the [run projection repair runbook](run-projection-repair.md).
