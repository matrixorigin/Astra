# Getting started with Astra

Choose the path that matches what you want to do. All paths use the same
durable agent backbone; the difference is where Astra runs and which capacity
providers are connected.

| Goal | Path |
| --- | --- |
| Evaluate Astra and use the CLI/TUI | [Build from source](#build-from-source) |
| Run the packaged service stack | [Docker quick start](docker.md) |
| Prepare a real deployment | [Production deployment](production.md) |
| Change Astra itself | [Developer setup](development.md) |

## Build from source

### Prerequisites

- Git and Make
- Docker with Docker Compose
- Rust via `rustup` (the repository pins the required toolchain)
- Node.js 24 (pinned in [`.nvmrc`](../../.nvmrc))
- OpenSSL command-line tools
- At least one supported LLM endpoint, plus either an embedding API for semantic
  memory or deterministic mock embeddings for local evaluation

### Initialize and start

```bash
git clone https://github.com/matrixorigin/Astra.git
cd Astra

cp .models.yaml.example .models.yaml
make dev-init
```

Configure `MEMORIA_EMBEDDING_BASE_URL` in `.env` for semantic memory and add
`MEMORIA_EMBEDDING_API_KEY` when the endpoint requires authentication, or set
`MEMORIA_EMBEDDING_PROVIDER=mock` for local evaluation. Then add credentials
for at least one model endpoint to `.models.yaml`. Never commit either local
file.

```bash
make build
make dev-start-server-only

export PATH="$PWD/target/release:$PATH"
astra health
```

The default development endpoints are:

- Web dashboard: <http://localhost:3536>
- HTTP API: <http://localhost:17001>
- Health check: <http://localhost:17001/health>

### Create the first account and model offering

```bash
astra admin register
astra admin model load .models.yaml --update-existing
astra admin model check <model-name>
```

Start the TUI or send a one-shot request:

```bash
astra
astra chat -m "Map this repository and explain its architecture"
```

Server-only mode deliberately has no ambient access to host files, processes,
or Git. Connect a User Runner when Web sessions need a local workspace:

```bash
ASTRA_EDGE_WORKSPACE_DIR=/path/to/workspace make dev-edge-start
```

On later starts, `make dev-start-server-edge` brings up the Server and local
User Runner together.

## Next steps

| Task | Documentation |
| --- | --- |
| Learn the CLI and TUI | [CLI commands](../reference/cli-commands.md) · [slash commands](../reference/slash-commands.md) |
| Integrate an application | [TypeScript SDK](../../packages/sdk/README.md) · [HTTP API](../reference/api-reference.md) |
| Understand settings | [Configuration reference](../reference/configuration.md) |
| Deploy Astra | [Deployment overview](../../deployment/README.md) |
| Develop Astra | [Development workflow](../guides/development-workflow.md) · [testing guide](../guides/testing.md) |
| Diagnose a problem | [Troubleshooting](../guides/troubleshooting.md) |

Return to the [documentation index](../README.md) for the complete user,
operator, contributor, and kernel-design map.
