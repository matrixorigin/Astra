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
authority`, the client is refusing to run without a kernel-backed execution
owner. On macOS this normally means `/dev/dtracehelper` is unavailable or is
not the expected root-owned device; on other platforms no supported authority
has been installed. Check `astra --version`, repair the host/runtime image or
use a supported Linux deployment, and retry. Do not bypass the lease or reuse a
binary built for a different platform.

## Stale Run Projection

If a run list or projection response looks stale while the durable run status is
correct, use the [run projection repair runbook](run-projection-repair.md).
