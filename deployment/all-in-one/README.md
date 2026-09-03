# All-in-One Deployment

This compose stack starts MatrixOne, Memoria, and `astra-server`.

The development flow is separate and still uses `docker-compose.deps.yml` through the repo-root `make dev-deps-*` targets, followed by `make dev-api-start` for a locally built API server.

## Start With Make

Install the released client binaries if they are not already available:

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/matrixorigin/Astra/main/scripts/install-astra.sh | sh
```

From the repository root, the recommended first-run path is guided:

```bash
make stack-setup
```

The guided path asks for embedding configuration, starts Compose, verifies a
memory round trip, and launches `astra admin setup` for the administrator and
LLM model. API keys are hidden while typing and the local `.env` is owner-only.
Choose a real endpoint for production retrieval or deterministic mock
embeddings for evaluation. The wizard works in Linux/macOS terminals and
Windows WSL or Git Bash; restricted Windows shells can use the non-interactive
targets and `astra admin setup`.

For a non-interactive local evaluation, use deterministic mock embeddings:

```bash
MEMORIA_EMBEDDING_PROVIDER=mock make stack-start
```

When configuring the file by hand, set `MEMORIA_EMBEDDING_BASE_URL` and, when
required, `MEMORIA_EMBEDDING_API_KEY`. For no-credential evaluation, set
`MEMORIA_EMBEDDING_PROVIDER=mock` and leave both blank.

`stack-start` creates `deployment/all-in-one/.env`, generates local secrets,
validates Compose, starts every service, waits for readiness, and verifies an
exact memory write/retrieval round trip. For lower-level automation, run
`make stack-env`, `make stack-up`, and `make stack-verify` explicitly.

`make stack-up` fails before starting containers if a required secret is empty,
or if a non-mock embedding provider is missing its base URL. An API key is
optional for unauthenticated local endpoints. If a service does not become
healthy, the command prints container status and recent logs while leaving the
partial stack available for inspection. Fix the first reported error and rerun
the command, or use `make stack-down` to stop it.

For semantic memory, supply a real OpenAI-compatible embedding endpoint; its
API key is optional when the endpoint is unauthenticated:

```bash
MEMORIA_EMBEDDING_BASE_URL=https://your-embedding-endpoint/v1 \
MEMORIA_EMBEDDING_API_KEY="$EMBEDDING_API_KEY" \
make stack-start
```

Process-level credentials are passed to Compose but are not written to `.env`.
Put embedding settings in that file only when persistence is intentional;
otherwise export them again on subsequent `make stack-up` invocations.
`make stack-verify` repeats the runtime proof.

If a service does not become healthy, startup prints container status and
recent logs while leaving the partial stack available for inspection. Fix the
first reported error and rerun `make stack-up`, or use `make stack-down`.

The complete operator loop is:

```bash
make stack-status
make stack-verify
make stack-logs SERVICE=api
make stack-down
```

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

Use `astra admin setup` for the guided administrator and model flow. It asks
whether the MatrixOne data volume is fresh, then bootstraps or signs in before
configuring the model. `astra admin register` and `astra admin model ...`
remain available for automation.

```bash
astra admin --api-url http://127.0.0.1:17001 register \
  --username admin \
  --email admin@example.com \
  --password '<password>'

astra admin --api-url http://127.0.0.1:17001 model load .models.yaml --update-existing
```

Connect the installed User Runner to expose one explicit local workspace:

```bash
astra-edge --server-url http://127.0.0.1:17001 \
  --workspace-dir /path/to/workspace
```

When building from source, use `./target/debug/astra` (or
`./target/release/astra`) in place of `astra` above. `--api-url` can be omitted
whenever the server listens on the default `http://127.0.0.1:17001`.

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
env UID="$(id -u)" GID="$(id -g)" \
  docker compose up -d --wait --wait-timeout 180
```

Passing the host UID and GID keeps bind-mounted service logs owned by the user
running Compose. The Make targets do this automatically.

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
make stack-verify
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
