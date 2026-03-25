# Deployment Overview

## Main Paths

- `deployment/all-in-one/` - local or single-host Docker compose deployment
- `deployment/kubernetes/` - cluster-oriented manifests and chart assets
- `scripts/ops/` - operational helpers

## Pre-Deployment Validation

```bash
make check
make test
```

## All-in-One Compose

```bash
cd deployment/all-in-one
docker compose up -d
docker compose --profile app up -d --build
```
