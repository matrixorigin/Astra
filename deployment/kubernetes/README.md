# Kubernetes Deployment

The Helm chart deploys Astra Server as a stateless Kubernetes workload. It
expects MatrixOne, Memoria, and provider credentials to be managed outside the
chart and injected through an existing Kubernetes Secret.

This boundary is intentional: the chart installs only resources it actually
owns. It does not create placeholder database, cache, inference, or worker
services without corresponding workloads.

## Runtime Profile

The chart is Server-only. Web agent, memory, planning, MCP, introspection,
trace, audit, and server-service tools can run in this profile, but the Server
does not gain implicit access to user files, shell, Git, private networks, or
attached hardware.

Connect `astra-edge` separately when a workspace needs a User Runner. Give each
Runner its own identity, token, workspace binding, and network boundary; do not
hide it inside the Server deployment.

## Prerequisites

- a currently supported Kubernetes cluster
- Helm 3 or newer
- reachable MatrixOne and Memoria services
- an Astra runtime image accessible from the cluster
- a namespace-scoped Secret containing runtime configuration

## Create Runtime Configuration

Create a local env file outside the repository and populate at least:

```dotenv
MATRIXONE_HOST=db.example.internal
MATRIXONE_PORT=6001
MATRIXONE_USER=astra
MATRIXONE_PASSWORD=...
ASTRA_DATABASE=astra_runtime
ASTRA_JWT_SECRET=...
ASTRA_TOKEN_ENCRYPTION_KEY=...
ASTRA_RUNTIME_ROOT_SECRET=...
ASTRA_CORS_ORIGINS=https://astra.example.com
MEMORIA_BASE_URL=https://memoria.example.internal
MEMORIA_MASTER_KEY=...
```

Create the namespace and Secret without committing that file:

```bash
kubectl create namespace astra
kubectl -n astra create secret generic astra-runtime \
  --from-env-file=/secure/path/astra-runtime.env
```

Model/provider configuration remains server-managed through `astra admin model
add` and `astra admin model check` after the Server is available.

## Install

```bash
helm upgrade --install astra ./chart \
  --namespace astra \
  --set api.existingSecret=astra-runtime
```

The default image is `matrixorigin/astra:<Chart.appVersion>`. Override the
repository and tag for a private registry or pinned internal build:

```bash
helm upgrade --install astra ./chart \
  --namespace astra \
  --set api.existingSecret=astra-runtime \
  --set api.image.repository=registry.example.com/platform/astra \
  --set api.image.tag=0.2.0
```

For an immutable deployment, set `api.image.digest` to a `sha256:...` digest;
when present, it takes precedence over the tag.

Use `imagePullSecrets` when the registry requires authentication. Keep
non-sensitive overrides in `api.env`; credentials belong in the existing
Secret.

## Verify

```bash
kubectl -n astra rollout status deployment/astra-api
kubectl -n astra port-forward service/astra-api 17001:17001
curl http://127.0.0.1:17001/health
curl http://127.0.0.1:17001/live
curl http://127.0.0.1:17001/ready
```

The liveness probe uses dependency-free `/live`; the readiness probe uses
`/ready`, so dependency failures stop traffic without restarting a healthy
Server process. `/health` remains the aggregate diagnostic view.

## Scale

The chart enables a CPU-based HorizontalPodAutoscaler by default, with two to
ten replicas. Adjust its bounds in a values file:

```yaml
api:
  hpa:
    minReplicas: 3
    maxReplicas: 20
    targetCPUUtilization: 70
```

Disable the HPA before controlling replicas directly:

```bash
helm upgrade astra ./chart \
  --namespace astra \
  --reuse-values \
  --set api.hpa.enabled=false \
  --set api.replicas=3
```
