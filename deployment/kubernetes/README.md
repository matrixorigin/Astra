# Kubernetes Deployment

Production deployment with Helm. All components except API are optional.

## Quick Start

```bash
# Minimal: API only (external DB + Redis)
helm install mo-agent ./chart \
  --set matrixone.enabled=false \
  --set matrixone.external.host=db.prod.internal \
  --set redis.enabled=false \
  --set redis.external.url=redis://redis.prod.internal:6379

# Full: everything in-cluster
helm install mo-agent ./chart
```

## Components

| Component | Default | Flag | Description |
|-----------|---------|------|-------------|
| API | ✅ Required | — | REST API, HPA 2-10 pods |
| MatrixOne | [opt] | `matrixone.enabled` | In-cluster DB, or use external |
| Redis | [opt] | `redis.enabled` | In-cluster Redis, or use external |
| Model Server | [opt] | `modelServer.enabled` | Shared inference for platform-trained small models (NOT LLMs) |
| Skill Worker | [opt] | `skillWorker.enabled` | K8s Jobs for heavy skills |
| GPU | [opt] | `skillWorker.gpu.enabled` | GPU node selector + tolerations |
| Ray | [opt] | `ray.enabled` | Distributed compute cluster |

## Helm Values

See `chart/values.yaml` for all options. Key settings:

```yaml
api:
  replicas: 2
  hpa:
    enabled: true
    maxReplicas: 10
  resources:
    requests:
      cpu: "500m"
      memory: "512Mi"

matrixone:
  enabled: true           # false → use external
  external:
    host: ""
    port: 6001

redis:
  enabled: true           # false → use external
  external:
    url: ""

modelServer:
  enabled: false
  replicas: 1

skillWorker:
  enabled: false
  gpu:
    enabled: false
    nodeSelector:
      accelerator: nvidia-gpu

ray:
  enabled: false
  workers:
    gpu:
      replicas: 0
    cpu:
      minReplicas: 1
      maxReplicas: 8
```

## Examples

```bash
# Dev cluster: all in-cluster, no GPU
helm install mo-agent ./chart

# Staging: external DB, model server enabled
helm install mo-agent ./chart \
  --set matrixone.enabled=false \
  --set matrixone.external.host=mo-staging.rds.internal \
  --set modelServer.enabled=true

# Production: external DB + Redis, GPU training, Ray
helm install mo-agent ./chart \
  --set matrixone.enabled=false \
  --set matrixone.external.host=mo-prod.rds.internal \
  --set redis.enabled=false \
  --set redis.external.url=redis://redis-prod.internal:6379 \
  --set modelServer.enabled=true \
  --set skillWorker.enabled=true \
  --set skillWorker.gpu.enabled=true \
  --set ray.enabled=true
```

## Scaling

```bash
# Scale API manually
kubectl scale deployment mo-agent-api --replicas=5

# Or let HPA handle it (default: CPU 70%)
# HPA auto-scales 2-10 pods based on CPU utilization
```
