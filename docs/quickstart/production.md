# Production Deployment

## Before You Deploy

```bash
make check
make test
```

## All-in-One Compose

```bash
make stack-env
make stack-up-server-only
```

For host-local workspace tools in Web sessions:

```bash
astra login
ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo make stack-up-server-edge
```

For Kubernetes, install the API chart as server-only, then connect `astra-edge`
as a separate provider process only for the workspaces or networks that should
be exposed.

## Required Configuration

- `ASTRA_TOKEN_ENCRYPTION_KEY`
- `ASTRA_JWT_SECRET`
- MatrixOne connection settings
- Memoria connection/settings
- any model/provider secrets you actually use

Use `.env.example` as the starting point.
