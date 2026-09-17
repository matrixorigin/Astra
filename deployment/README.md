# Deployment Overview

## Main Paths

- `deployment/all-in-one/` - local or single-host Docker compose deployment
- `deployment/kubernetes/` - cluster-oriented manifests and chart assets
- `deployment/monitoring/` - optional Prometheus and Grafana stack
- `scripts/ops/` - operational helpers

## Image Build Paths

The release workflow builds multi-architecture images from the root
[`Dockerfile`](../Dockerfile). Use this path for normal development and
published releases.

[`Dockerfile.prebuilt`](../Dockerfile.prebuilt) is a specialized amd64 path for
packaging already cross-compiled `astra-server` and `astra` binaries without
running a Rust build inside Docker. Its colocated
[`Dockerfile.prebuilt.dockerignore`](../Dockerfile.prebuilt.dockerignore) keeps
that build context limited to the two binaries and the license. Follow the
prerequisite and build commands in the Dockerfile header; it is not used by the
standard release workflow.

## Pre-Deployment Validation

```bash
make check
make test
make test-runtime-profiles
```

`test-runtime-profiles` is the focused guardrail for the two supported runtime
startup shapes: server-only and server+edge.

## All-in-One Compose

For a first local evaluation, install the released CLI and User Runner, then
start and verify the complete published stack in one command from the
repository root:

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/matrixorigin/Astra/main/scripts/install-astra.sh | sh -s -- --dir "$HOME/.local/bin"
export PATH="$HOME/.local/bin:$PATH"
ASTRA_VERSION="$(astra --version | awk '{print $2}')"
git clone --branch "v${ASTRA_VERSION}" --depth 1 https://github.com/matrixorigin/Astra.git "Astra-${ASTRA_VERSION}"
cd "Astra-${ASTRA_VERSION}"
MEMORIA_EMBEDDING_PROVIDER=mock make stack-start
```

`stack-start` generates local secrets, starts MatrixOne, Memoria, and
`astra-server`, waits for readiness, and verifies a memory round trip. It does
not connect a local filesystem/process provider. Use a real embedding endpoint
instead of mock mode for semantic-memory evaluation or production.
The checked-out release pins all three service images to one tested
compatibility set.

On later starts, `make stack-up` reuses the generated secrets. Repeat any
process-level embedding settings, or persist them in the stack `.env` file
deliberately. Use `make stack-verify` to repeat the runtime proof.

For Web sessions that need local files, shell, git, or private-network access,
start the same stack plus a host `astra-edge` provider:

```bash
make stack-up-server-edge
```

`astra-edge` reads the selected Astra CLI profile token by default. Run
`astra login` first, or set `ASTRA_TOKEN` explicitly. Use
`ASTRA_EDGE_WORKSPACE_DIR=/path/to/repo` to choose the local workspace exposed
to the server runtime.
