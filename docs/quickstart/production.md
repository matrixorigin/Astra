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
