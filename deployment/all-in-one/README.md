# All-in-One Deployment (Docker Compose)

One command to start everything.

## Quick Start

```bash
# From project root:
docker-compose up -d
```

## Profiles

```bash
# Core only: MatrixOne + Redis + Init + API
docker-compose up -d

# + GPU training worker
docker-compose --profile gpu up -d

# + Shared model inference server
docker-compose --profile model up -d

# Everything
docker-compose --profile full up -d
```

## Services

| Service | Port | Profile | Description |
|---------|------|---------|-------------|
| matrixone | 6001 | default | HTAP database |
| redis | 6379 | default | Cache, queue |
| init | — | default | DB schema + prompts (run once) |
| api | 8000 | default | REST API server |
| skill-worker | — | gpu | GPU training tasks |
| model-server | 9527 | model | Shared inference for platform-trained small models (NOT LLMs) |

## Startup Sequence

```
matrixone + redis (healthcheck)
    → init (schema + prompts, exits on success)
    → api + skill-worker + model-server
```

## Configuration

Copy `.env.example` to `.env` and edit:

```bash
# Required
JWT_SECRET=your-secret-here

# Optional
API_PORT=8000
API_WORKERS=2
MATRIXONE_PORT=6001
MATRIXONE_DATABASE=mo_dev_agent
REDIS_PORT=6379
MODEL_SERVER_PORT=9527
```

## GPU Support

Requires [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/install-guide.html):

```bash
# Install nvidia-container-toolkit, then:
docker-compose --profile gpu up -d
```
