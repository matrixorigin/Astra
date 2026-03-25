# Troubleshooting Guide

## Quick Checks

```bash
make dev-status
make dev-api-logs
make dev-deps-logs
curl http://localhost:8000/health
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
make test-integration
make test
make check
```
