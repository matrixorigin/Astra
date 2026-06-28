# All-in-One Deployment

This compose stack starts MatrixOne, Memoria, and `astra-server`.

The development flow is separate and still uses `docker-compose.deps.yml` through the repo-root `make dev-deps-*` targets, followed by `make dev-api-start` for a locally built API server.

## Start With Make

From the repo root:

```bash
make stack-env
```

Fill the required embedding configuration in `deployment/all-in-one/.env`:

- `MEMORIA_EMBEDDING_API_KEY`
- `MEMORIA_EMBEDDING_BASE_URL`

Then start the stack:

```bash
make stack-up
```

`make stack-up` fails before starting containers if either required embedding value is empty.

## Admin Accounts

Use `astra-admin register` to create an administrator account. On a fresh MatrixOne
data volume this performs the initial admin bootstrap. After an admin exists,
`astra-admin register` must be run while logged in as an existing admin.

```bash
./rust/target/debug/astra-admin --api-url http://127.0.0.1:17001 register \
  --username admin \
  --email admin@example.com \
  --password '<password>'

./rust/target/debug/astra-admin --api-url http://127.0.0.1:17001 model load .models.yaml --update-existing
```

`astra-admin register` stores the returned admin credentials locally. It prints
`registered and logged in (initial admin)` for the first admin, and
`registered and logged in (admin)` when an existing admin creates another admin.

Use `astra register` for regular, non-admin users. If a previous smoke test created
the initial admin and you want to replay the fresh bootstrap path, reset the
MatrixOne volume:

```bash
make stack-clean
```

## Required Configuration

Required for startup:

- `MEMORIA_EMBEDDING_API_KEY`
- `MEMORIA_EMBEDDING_BASE_URL`

The stack provides local-development defaults for these internal secrets:

- `ASTRA_JWT_SECRET`
- `ASTRA_TOKEN_ENCRYPTION_KEY`
- `ASTRA_BRIDGE_SECRET`
- `MEMORIA_MASTER_KEY`

Change those defaults before exposing the stack outside a trusted development environment.

## Start With Docker Compose

```bash
cd deployment/all-in-one
cp .env.example .env
```

Fill the required embedding configuration in `.env`:

- `MEMORIA_EMBEDDING_API_KEY`
- `MEMORIA_EMBEDDING_BASE_URL`

Then start the stack:

```bash
docker compose up -d
```

## Services

| Service | Host port | Description |
| --- | --- | --- |
| `api` | `17001` | `astra-server` HTTP API |
| `memoria` | `8100` | Memoria memory service |
| `matrixone` | `26001` | MatrixOne MySQL-compatible endpoint |
| `matrixone` debug | `26060` | MatrixOne debug/health endpoint |

`ASTRA_API_PORT` in `.env` controls the host-facing published port. The API container itself listens on `17001`.

## Images

By default the stack pulls:

- `matrixorigin/astra:latest`
- `matrixorigin/memoria:latest`
- `matrixorigin/matrixone:latest`

Override image tags in `.env`, for example:

```dotenv
ASTRA_IMAGE=matrixorigin/astra:0.1.0
MEMORIA_IMAGE=matrixorigin/memoria:latest
```

## Operations

```bash
make stack-status
make stack-logs SERVICE=api

docker compose ps
docker compose logs -f api
docker compose logs -f memoria
curl http://localhost:17001/health
```

Stop the stack:

```bash
make stack-down

docker compose down
```

Remove MatrixOne data as well:

```bash
make stack-clean
```
