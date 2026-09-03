# All-in-One Deployment

This compose stack starts MatrixOne, Memoria, and `astra-server`.

The development flow is separate and still uses `docker-compose.deps.yml` through the repo-root `make dev-deps-*` targets, followed by `make dev-api-start` for a locally built API server.

## Start With Make

From the repo root:

```bash
make stack-env
```

`make stack-env` creates `deployment/all-in-one/.env` and generates local
stack secrets. For semantic memory, configure:

- `MEMORIA_EMBEDDING_BASE_URL`
- `MEMORIA_EMBEDDING_API_KEY` when the endpoint requires authentication

For a no-credential local evaluation, set `MEMORIA_EMBEDDING_PROVIDER=mock`
and leave both values blank. Mock embeddings are deterministic and suitable for
evaluation or tests, not production retrieval.

Then start the stack:

```bash
make stack-up
```

`make stack-up` fails before starting containers if a required secret is empty,
or if a non-mock embedding provider is missing its base URL. An API key is
optional for unauthenticated local endpoints.

## Runtime Startup Profiles

The compose stack is server-only by default: it starts MatrixOne, Memoria, and
`astra-server`. Web agent, memory, planning, MCP, introspection, and other
server-service capabilities can run in this mode, but local filesystem, shell,
git, build/test, and private-network tools are not available unless an execution
provider connects.

Use the explicit server-only target when that is the intended test/deployment
shape:

```bash
make stack-up-server-only
```

This target also stops any local `astra-edge` process started by the repo dev
scripts before starting the compose stack, so the resulting process graph is
actually server-only instead of "server plus a previously connected local edge".

Use server+edge when a Web session should operate on a local workspace from the
host machine:

```bash
astra login
ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo make stack-up-server-edge
```

`stack-up-server-edge` starts the same compose stack and then launches a local
host `astra-edge` process connected to `/edge/ws`. The edge process reads the
selected Astra CLI profile token by default; set `ASTRA_TOKEN` if you need to
inject a token explicitly.

Run the focused runtime profile guardrails before changing startup or tool
visibility behavior:

```bash
make test-runtime-profiles
```

## Admin Accounts

Use `astra admin register` to create an administrator account. On a fresh MatrixOne
data volume this performs the initial admin bootstrap. After an admin exists,
`astra admin register` must be run while logged in as an existing admin.

```bash
./target/debug/astra admin --api-url http://127.0.0.1:17001 register \
  --username admin \
  --email admin@example.com \
  --password '<password>'

./target/debug/astra admin --api-url http://127.0.0.1:17001 model load .models.yaml --update-existing
```

`astra admin register` stores the returned admin credentials locally. It prints
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

- `ASTRA_JWT_SECRET`
- `ASTRA_TOKEN_ENCRYPTION_KEY`
- `ASTRA_RUNTIME_ROOT_SECRET`
- `MEMORIA_MASTER_KEY`

For non-mock embeddings, also configure:

- `MEMORIA_EMBEDDING_BASE_URL`
- `MEMORIA_EMBEDDING_API_KEY` when the endpoint requires authentication

The Makefile generates the four secret values for local single-host bring-up.
When using plain `docker compose`, generate and fill them yourself instead of
leaving the template values empty.

## Start With Docker Compose

```bash
cd deployment/all-in-one
cp .env.example .env
```

Fill the required configuration in `.env`:

- `ASTRA_JWT_SECRET`
- `ASTRA_TOKEN_ENCRYPTION_KEY`
- `ASTRA_RUNTIME_ROOT_SECRET`
- `MEMORIA_MASTER_KEY`

For semantic memory, configure `MEMORIA_EMBEDDING_BASE_URL` and add
`MEMORIA_EMBEDDING_API_KEY` when the endpoint requires it. Alternatively, set
`MEMORIA_EMBEDDING_PROVIDER=mock` for deterministic local evaluation and leave
both values blank.

Then start the stack:

```bash
docker compose up -d
```

## Services

| Service           | Host port | Description                         |
| ----------------- | --------- | ----------------------------------- |
| `api`             | `17001`   | `astra-server` HTTP API             |
| `memoria`         | `8100`    | Memoria memory service              |
| `matrixone`       | `26001`   | MatrixOne MySQL-compatible endpoint |
| `matrixone` debug | `26060`   | MatrixOne debug/health endpoint     |

All published ports bind to `127.0.0.1` by default. `ASTRA_BIND_ADDRESS` changes
that interface, and `ASTRA_API_PORT` controls the host-facing API port. The API
container itself listens on `17001`. Do not use a non-loopback bind with the
development credentials on an untrusted network.

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
