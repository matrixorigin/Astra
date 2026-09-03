# Deployment Guide

## Deployment Profiles

For local or single-host evaluation, use the all-in-one Compose stack:

```bash
make stack-up-server-only
```

Production deployments use externally managed MatrixOne and Memoria services,
an immutable Astra image, and the production Server-only profile:

```bash
cp .env.production.example .env.production
# Populate .env.production, then validate and start the profile.
./scripts/ops/deploy.sh 3
```

See the [production deployment guide](../quickstart/production.md) for the
required configuration and trust boundary.

If the deployment also needs host-local files, shell, git, or private-network
access from Web sessions, connect a host edge provider:

```bash
ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo make stack-up-server-edge
```

For Kubernetes, the Helm chart is Server-only by default. Deploy edge capacity
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
- `deployment/all-in-one/docker-compose.prod.yml`
- `deployment/all-in-one/.env.example`
- `.env.production.example`
- `scripts/ops/deploy.sh`
- `scripts/ops/validate_production_env.sh`
