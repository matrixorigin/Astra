# Production Deployment

## Before You Deploy

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

## Required Configuration

- `ASTRA_TOKEN_ENCRYPTION_KEY`
- `ASTRA_JWT_SECRET`
- MatrixOne connection settings
- Redis connection settings
- any model/provider secrets you actually use

Use `.env.example` as the starting point.
