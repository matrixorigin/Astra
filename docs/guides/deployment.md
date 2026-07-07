# Deployment Guide

## Recommended Path

For the checked-in deployment assets, use the all-in-one compose stack:

```bash
make stack-up-server-only
```

If the deployment also needs host-local files, shell, git, or private-network
access from Web sessions, connect a host edge provider:

```bash
ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo make stack-up-server-edge
```

For Kubernetes, the Helm chart is server-only by default. Deploy edge capacity
as a separate `astra-edge` provider process when a workspace or private network
must be exposed to Web sessions.

## Before Deployment

```bash
make check
make test
```

## Runtime Verification

```bash
curl http://localhost:17001/health
```

## Related Files

- `deployment/all-in-one/docker-compose.yml`
- `deployment/all-in-one/.env.example`
- `scripts/ops/deploy.sh`
