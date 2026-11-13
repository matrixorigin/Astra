# Production Deployment

Production deployments run Astra Server against externally managed MatrixOne
and Memoria services. Do not promote the development All-in-One stack, its
embedded database credentials, or a mutable `latest` image into production.

## Before You Deploy

```bash
make check
make test
```

Use an immutable Astra release tag or image digest and replace every
`CHANGE_ME_*` value in `.env.production.example`. Inject the populated values
through your deployment platform; never commit the resulting file. The
checked-in deployment helper rejects missing values, template placeholders,
mutable or implicit image tags, wildcard CORS, and undersized secrets before
starting Compose.

## Single Host

The production Compose profile runs Astra Server behind Nginx. It does not
start databases or memory services:

```bash
cp .env.production.example .env.production
# Edit .env.production and set external MatrixOne, Memoria, CORS, and secrets.

./scripts/ops/deploy.sh 3
```

The helper validates the environment without evaluating it as shell code, then
validates and starts the Compose profile. If you need to operate Compose
directly, keep the same validation gate:

```bash
./scripts/ops/validate_production_env.sh .env.production
cd deployment/all-in-one
docker compose --env-file ../../.env.production \
  -f docker-compose.prod.yml config --quiet
docker compose --env-file ../../.env.production \
  -f docker-compose.prod.yml up -d --scale api=3
```

The API containers are reachable only through the Nginx service, so scaling
does not create conflicting host ports. Terminate TLS at a trusted external
load balancer or extend `nginx.conf` with your managed certificate before
exposing the endpoint to users.

## Kubernetes

The [Kubernetes guide](../../deployment/kubernetes/README.md) deploys the same
Server-only boundary. It requires an existing namespace Secret for runtime
configuration and does not install placeholder dependencies.

## Required Configuration

- an immutable `ASTRA_IMAGE`
- `ASTRA_TOKEN_ENCRYPTION_KEY`
- `ASTRA_JWT_SECRET`
- `ASTRA_RUNTIME_ROOT_SECRET`
- an explicit `ASTRA_CORS_ORIGINS`
- external MatrixOne host, user, password, and database
- external `MEMORIA_BASE_URL` and `MEMORIA_MASTER_KEY`
- any model/provider credentials actually used

## User Runners

Keep workspace and private-network execution outside the Server deployment.
Connect `astra-edge` as a separate User Runner only for the workspaces or
networks that should be exposed, using a dedicated identity and token for each
Runner boundary.

The MOI connect command (`astra-edge --edge-id runner-… --server-url … --token … --reconnect true`)
detaches when it is started from a terminal. After it returns, the terminal can
be closed. `ASTRA_EDGE_FOREGROUND=1` (also `true` or `yes`) keeps that same
command in the foreground. Containers, sandbox processes, and non-interactive
starts stay in the foreground.

The command prints `status`, `logs`, and `stop` with the installed executable
path and shell quoting. Run those printed commands; a later shell does not need
`astra-edge` on `PATH`. Running the connect command again leaves the background
runner in place and hands it the command's credential, which is used from its
next connection attempt, including an attempt whose old credential was just
rejected; work already admitted keeps running. If a new credential cannot
authenticate, the runner continues with the credential that last authenticated,
and the same command can be run again later. A runner that is stopping does not
accept a new credential, including before it has finished leaving the connected
state; run the command again after it stops. `stop` waits for a start that has
not recorded its metadata yet. Once the runner process exists, `stop` stops it
without waiting for authentication. The runner stays up only after the starting
command accepts a connection that is still up. Closing the terminal, or losing
that command before it accepts, cancels the startup, including while the runner
is still trying to reach the server. Work the runner has already started is
cancelled and its result is kept for the next connection. The token is not
written to the runner metadata. Each background runner keeps the credential it
last authenticated for its own identity, and the execution record for its own
server, so another runner in the same workspace does not replace either. A
record written before records were separated by server stays with the server
that next opens it. If that older record and a server-specific record both
exist, startup stops and names both files instead of dropping either one.
