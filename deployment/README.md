# Deployment

Two deployment modes, choose based on your environment.

## [all-in-one/](all-in-one/) — Docker Compose

One command, everything included. Best for development, staging, and small production.

```bash
docker-compose up -d                   # Core services
docker-compose --profile full up -d    # Everything (+ GPU + Model Server)
```

## [kubernetes/](kubernetes/) — Helm Chart

Production-grade. All components except API are optional — use your existing DB/Redis/GPU infra.

```bash
helm install mo-agent ./kubernetes/chart              # Full in-cluster
helm install mo-agent ./kubernetes/chart \
  --set matrixone.enabled=false \
  --set matrixone.external.host=db.prod.internal      # External DB
```

## Architecture

See [docs/design/deployment-architecture.md](../docs/design/deployment-architecture.md) for the full design.

```
                        ┌──────────────┐
                        │   Clients    │
                        └──────┬───────┘
                               │
                        ┌──────▼───────┐
                        │  API Server  │  ← Required
                        │              │──→ LLM APIs (DeepSeek, OpenAI, ...)
                        └──────┬───────┘
                               │
              ┌────────────────┼────────────────┐
              │                │                │
       ┌──────▼──────┐ ┌──────▼──────┐ ┌───────▼───────┐
       │  MatrixOne   │ │    Redis    │ │ Model Server  │
       │   [opt]      │ │    [opt]    │ │ [opt] small   │
       └─────────────┘ └─────────────┘ │ models only   │
                                        └───────────────┘
                                              │
                               ┌──────────────┼──────────────┐
                               │              │              │
                        ┌──────▼──────┐ ┌─────▼─────┐ ┌─────▼─────┐
                        │Skill Worker │ │ Ray Cluster│ │ K8s Jobs  │
                        │  [opt:gpu]  │ │   [opt]   │ │   [opt]   │
                        └─────────────┘ └───────────┘ └───────────┘
```
