# Deployment

Production-ready deployment configurations for mo-agent.

## Quick Start

### Development

```bash
# Start dependencies only (MatrixOne + Redis)
docker-compose -f all-in-one/docker-compose.yml up -d

# Or start everything
make dev-start
```

### Production

```bash
# Docker Compose (recommended for small-medium scale)
cd all-in-one
cp .env.example .env
# Edit .env with your settings
docker-compose -f docker-compose.prod.yml up -d

# Kubernetes (recommended for large scale)
helm install mo-agent kubernetes/chart -f values.prod.yaml
```

---

## Deployment Options

### 1. Docker Compose (Small-Medium Scale)

**Best for:**
- Development and staging
- Small production deployments (< 1000 req/s)
- Single server setups

**Features:**
- Simple setup (one command)
- All services included
- Easy to debug
- Resource limits and health checks

**See:** [all-in-one/](all-in-one/)

### 2. Kubernetes (Large Scale)

**Best for:**
- Production deployments
- High availability requirements
- Auto-scaling needs (> 1000 req/s)
- Enterprise environments

**Features:**
- High availability
- Auto-scaling
- Rolling updates
- Resource management

**See:** [kubernetes/](kubernetes/)

### 3. Cloud Platforms

**Supported platforms:**
- AWS (ECS, EKS)
- GCP (Cloud Run, GKE)
- Azure (Container Instances, AKS)

**See:** [examples/](examples/)

---

## Directory Structure

```
deployment/
├── all-in-one/                  # Docker Compose deployments
│   ├── docker-compose.yml      # Development (dependencies only)
│   ├── docker-compose.prod.yml # Production (full stack)
│   ├── .env.example            # Environment template
│   └── nginx.conf              # Load balancer config
│
├── kubernetes/                  # Kubernetes deployments
│   ├── chart/                  # Helm chart
│   └── README.md
│
├── monitoring/                  # Monitoring stack
│   ├── docker-compose.yml      # Prometheus + Grafana
│   ├── prometheus.yml          # Prometheus config
│   ├── dashboards/             # Grafana dashboards
│   └── README.md
│
└── examples/                    # Cloud platform examples
    ├── aws/                    # AWS deployment
    ├── gcp/                    # GCP deployment
    └── on-premise/             # On-premise deployment
```

---

## Architecture

### Development

```
┌─────────────────────────────────────────────────────────┐
│                    Development                           │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │   Local    │─────▶│   Docker Compose         │      │
│  │   API      │      │   ┌──────────┐           │      │
│  │ (Python)   │      │   │MatrixOne │           │      │
│  └────────────┘      │   └──────────┘           │      │
│                      │   ┌──────────┐           │      │
│                      │   │  Redis   │           │      │
│                      │   └──────────┘           │      │
│                      └──────────────────────────┘      │
└─────────────────────────────────────────────────────────┘
```

### Production (Docker Compose)

```
┌─────────────────────────────────────────────────────────┐
│                    Production                            │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │   Nginx    │─────▶│   API Servers (3x)       │      │
│  │    LB      │      │   Docker Containers      │      │
│  └────────────┘      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ MatrixOne  │      │        Redis             │      │
│  │  Cluster   │      │       Cluster            │      │
│  └────────────┘      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ Prometheus │      │       Grafana            │      │
│  └────────────┘      └──────────────────────────┘      │
└─────────────────────────────────────────────────────────┘
```

### Production (Kubernetes)

```
┌─────────────────────────────────────────────────────────┐
│                  Kubernetes Cluster                      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │  Ingress   │─────▶│   API Deployment         │      │
│  │            │      │   ┌──────┐  ┌──────┐     │      │
│  │  (Nginx)   │      │   │ Pod  │  │ Pod  │     │      │
│  └────────────┘      │   └──────┘  └──────┘     │      │
│                      │   HPA (2-10 replicas)    │      │
│                      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ StatefulSet│      │      StatefulSet         │      │
│  │ MatrixOne  │      │        Redis             │      │
│  └────────────┘      └──────────────────────────┘      │
└─────────────────────────────────────────────────────────┘
```

---

## Configuration

### Environment Variables

All deployments use environment variables for configuration.

**Required:**
- `TOKEN_ENCRYPTION_KEY` - Encryption key for API tokens
- `JWT_SECRET_KEY` - JWT signing secret
- `LLM_PROVIDER` - LLM provider (openai, anthropic, etc.)
- `LLM_MODEL` - Model name
- `OPENAI_API_KEY` - OpenAI API key (if using OpenAI)

**See:** [all-in-one/.env.example](all-in-one/.env.example) for complete list

### Secrets Management

**Development:**
- Use `.env` file
- Auto-generate keys with `make dev-init`

**Production:**
- Use secrets management service:
  - AWS Secrets Manager
  - GCP Secret Manager
  - Kubernetes Secrets
  - HashiCorp Vault

---

## Monitoring

### Prometheus + Grafana

```bash
# Start monitoring stack
cd monitoring
docker-compose up -d

# Access
open http://localhost:9091  # Prometheus
open http://localhost:3000  # Grafana (admin/admin)
```

**Metrics collected:**
- API request rate and latency
- Database connection pool
- LLM request metrics
- System metrics

**See:** [monitoring/README.md](monitoring/README.md)

---

## Scaling

### Horizontal Scaling

**Docker Compose:**
```bash
docker-compose -f all-in-one/docker-compose.prod.yml up -d --scale api=5
```

**Kubernetes:**
```bash
kubectl scale deployment mo-agent-api --replicas=5
```

### Auto-Scaling (Kubernetes)

```yaml
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: mo-agent-api
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: mo-agent-api
  minReplicas: 2
  maxReplicas: 10
  metrics:
  - type: Resource
    resource:
      name: cpu
      target:
        type: Utilization
        averageUtilization: 70
```

---

## Backup and Recovery

### Database Backup

```bash
# Automated backup
./scripts/ops/backup.sh

# Manual backup
docker exec matrixone mysqldump \
  -h127.0.0.1 -P6001 -uroot -p111 \
  mo_agent > backup.sql
```

### Restore

```bash
# Restore from backup
./scripts/ops/restore.sh backup.sql.gz
```

**See:** [scripts/ops/](../scripts/ops/)

---

## Security

### Pre-Deployment Checklist

```bash
# Run security check
python scripts/security/check_security.py
```

**Verify:**
- ✅ Strong encryption keys
- ✅ No default passwords
- ✅ HTTPS enabled
- ✅ CORS properly configured
- ✅ Rate limiting enabled
- ✅ Secrets in environment variables

### SSL/TLS

**Nginx:**
```nginx
server {
    listen 443 ssl http2;
    ssl_certificate /etc/ssl/certs/cert.pem;
    ssl_certificate_key /etc/ssl/private/key.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
}
```

**Kubernetes:**
```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: mo-agent
spec:
  tls:
  - hosts:
    - api.your-domain.com
    secretName: tls-secret
```

---

## Troubleshooting

### Services Not Starting

```bash
# Check logs
docker-compose logs api
kubectl logs deployment/mo-agent-api

# Check health
curl http://localhost:8000/health
```

### Database Connection Issues

```bash
# Test connection
docker exec matrixone mysql -h127.0.0.1 -P6001 -uroot -p111 -e "SELECT 1"

# Check network
docker network inspect all-in-one_mo-net
```

### High CPU/Memory Usage

```bash
# Check resource usage
docker stats
kubectl top pods

# Scale up
docker-compose up -d --scale api=5
kubectl scale deployment mo-agent-api --replicas=5
```

**See:** [docs/guides/troubleshooting.md](../docs/guides/troubleshooting.md)

---

## See Also

- [Development Workflow](../docs/guides/development-workflow.md) - Development guide
- [Deployment Guide](../docs/guides/deployment.md) - Detailed deployment guide
- [Configuration Reference](../docs/reference/configuration.md) - All configuration options
- [Production Deployment](../docs/quickstart/production.md) - Production setup guide

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
