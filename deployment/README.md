# Deployment Overview

## Main Paths

- `deployment/all-in-one/` - local or single-host Docker compose deployment
- `deployment/kubernetes/` - cluster-oriented manifests and chart assets
- `scripts/ops/` - operational helpers

## Pre-Deployment Validation

```bash
make check
make test
make test-runtime-profiles
```

`test-runtime-profiles` is the focused guardrail for the two supported runtime
startup shapes: server-only and server+edge.

## All-in-One Compose

```bash
make stack-up-server-only
```

`stack-up-server-only` starts MatrixOne, Memoria, and `astra-server`. It does
not connect a local filesystem/process provider.

For Web sessions that need local files, shell, git, or private-network access,
start the same stack plus a host `astra-edge` provider:

```bash
make stack-up-server-edge
```

`astra-edge` reads the selected Astra CLI profile token by default. Run
`astra login` first, or set `ASTRA_TOKEN` explicitly. Use
`ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo` to choose the local workspace exposed
to the server runtime.
