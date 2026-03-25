# Deployment Guide

## Recommended Path

For the checked-in deployment assets, use the all-in-one compose stack:

```bash
cd deployment/all-in-one
docker compose up -d
docker compose --profile app up -d --build
```

## Before Deployment

```bash
make check
make test
```

## Runtime Verification

```bash
curl http://localhost:8000/health
```

## Related Files

- `deployment/all-in-one/docker-compose.yml`
- `deployment/all-in-one/.env.example`
- `scripts/ops/deploy.sh`
