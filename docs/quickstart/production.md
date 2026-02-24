# Production Deployment

Deploy mo-agent to production environments.

## Prerequisites

- Docker and Docker Compose (for containerized deployment)
- Kubernetes cluster (for K8s deployment)
- Production-grade database (MatrixOne cluster)
- Redis cluster or managed Redis
- Load balancer (nginx, AWS ALB, etc.)
- SSL certificates

## Deployment Options

### 1. Docker Compose (Small to Medium Scale)

Best for: Single server, small teams, development/staging

```bash
# 1. Prepare environment
cp .env.example .env.prod
# Edit .env.prod with production settings

# 2. Build production image
docker build -t mo-agent:latest -f Dockerfile .

# 3. Start services
docker-compose -f deployment/all-in-one/docker-compose.prod.yml up -d

# 4. Verify
curl https://your-domain.com/health
```

### 2. Kubernetes (Large Scale)

Best for: High availability, auto-scaling, enterprise

```bash
# 1. Configure Helm values
cp deployment/kubernetes/chart/values.yaml values.prod.yaml
# Edit values.prod.yaml

# 2. Install
helm install mo-agent deployment/kubernetes/chart \
  -f values.prod.yaml \
  --namespace mo-agent \
  --create-namespace

# 3. Verify
kubectl get pods -n mo-agent
```

### 3. Cloud Platforms

See platform-specific guides:
- [AWS Deployment](../guides/deployment.md#aws)
- [GCP Deployment](../guides/deployment.md#gcp)
- [Azure Deployment](../guides/deployment.md#azure)

## Production Configuration

### Environment Variables

**Security (Required):**
```bash
TOKEN_ENCRYPTION_KEY=<strong-random-key>
JWT_SECRET_KEY=<strong-random-key>
JWT_ACCESS_TOKEN_EXPIRE_MINUTES=30
JWT_REFRESH_TOKEN_EXPIRE_DAYS=7
```

**LLM Configuration:**
```bash
LLM_PROVIDER=openai
LLM_MODEL=gpt-4
OPENAI_API_KEY=<your-key>
LLM_TIMEOUT=60
LLM_MAX_RETRIES=3
```

**Database:**
```bash
MATRIXONE_HOST=matrixone-cluster.internal
MATRIXONE_PORT=6001
MATRIXONE_USER=mo_agent_user
MATRIXONE_PASSWORD=<strong-password>
MATRIXONE_DATABASE=mo_agent
MATRIXONE_POOL_SIZE=20
MATRIXONE_MAX_OVERFLOW=10
```

**Redis:**
```bash
REDIS_HOST=redis-cluster.internal
REDIS_PORT=6379
REDIS_PASSWORD=<strong-password>
REDIS_DB=0
REDIS_POOL_SIZE=50
```

**API:**
```bash
API_HOST=0.0.0.0
API_PORT=8000
API_WORKERS=4
LOG_LEVEL=info
CORS_ORIGINS=https://your-frontend.com
```

### Security Checklist

Before deploying to production:

```bash
# Run security check
python scripts/check_security.py

# Verify:
# ✅ Strong encryption keys
# ✅ No default passwords
# ✅ HTTPS enabled
# ✅ CORS configured
# ✅ Rate limiting enabled
# ✅ API keys not in code
```

## High Availability Setup

### Load Balancing

**Nginx Configuration:**
```nginx
upstream mo_agent_api {
    least_conn;
    server api-1:8000 max_fails=3 fail_timeout=30s;
    server api-2:8000 max_fails=3 fail_timeout=30s;
    server api-3:8000 max_fails=3 fail_timeout=30s;
}

server {
    listen 443 ssl http2;
    server_name api.your-domain.com;

    ssl_certificate /etc/ssl/certs/your-cert.pem;
    ssl_certificate_key /etc/ssl/private/your-key.pem;

    location / {
        proxy_pass http://mo_agent_api;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

### Database Clustering

Use MatrixOne cluster for high availability:

```yaml
# MatrixOne cluster configuration
matrixone:
  replicas: 3
  resources:
    limits:
      cpu: 4
      memory: 8Gi
  persistence:
    enabled: true
    size: 100Gi
```

### Redis Clustering

Use Redis Cluster or managed Redis:

```bash
# Redis Cluster
REDIS_CLUSTER_NODES=redis-1:6379,redis-2:6379,redis-3:6379

# Or AWS ElastiCache
REDIS_HOST=your-cluster.cache.amazonaws.com
REDIS_PORT=6379
```

## Monitoring and Observability

### Health Checks

```bash
# API health
curl https://api.your-domain.com/health

# Database health
curl https://api.your-domain.com/health/db

# Detailed metrics
curl https://api.your-domain.com/metrics
```

### Logging

Configure structured logging:

```bash
LOG_LEVEL=info
LOG_FORMAT=json
LOG_OUTPUT=/var/log/mo-agent/api.log
```

### Metrics

Expose Prometheus metrics:

```bash
# Enable metrics endpoint
ENABLE_METRICS=true
METRICS_PORT=9090

# Scrape configuration
curl http://localhost:9090/metrics
```

### Monitoring Stack

Deploy monitoring services:

```bash
# Prometheus + Grafana
docker-compose -f deployment/monitoring/docker-compose.yml up -d

# Access Grafana
open http://localhost:3000
```

## Backup and Recovery

### Database Backup

```bash
# Automated backup script
./scripts/ops/backup.sh

# Manual backup
docker exec matrixone mysqldump \
  -h127.0.0.1 -P6001 -uroot -p111 \
  mo_agent > backup-$(date +%Y%m%d).sql
```

### Restore

```bash
# Restore from backup
./scripts/ops/restore.sh backup-20260224.sql

# Manual restore
docker exec -i matrixone mysql \
  -h127.0.0.1 -P6001 -uroot -p111 \
  mo_agent < backup-20260224.sql
```

### Disaster Recovery

1. **Regular backups**: Daily automated backups
2. **Off-site storage**: Store backups in S3/GCS
3. **Test restores**: Monthly restore tests
4. **Failover plan**: Document failover procedures

## Scaling

### Horizontal Scaling

Scale API servers:

```bash
# Docker Compose
docker-compose -f deployment/all-in-one/docker-compose.prod.yml \
  up -d --scale api=5

# Kubernetes
kubectl scale deployment mo-agent-api --replicas=5 -n mo-agent
```

### Vertical Scaling

Increase resources per instance:

```yaml
# docker-compose.prod.yml
services:
  api:
    deploy:
      resources:
        limits:
          cpus: '4'
          memory: 8G
```

### Auto-scaling (Kubernetes)

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

## Maintenance

### Rolling Updates

```bash
# Kubernetes
kubectl set image deployment/mo-agent-api \
  api=mo-agent:v1.2.0 -n mo-agent

# Docker Compose
docker-compose -f deployment/all-in-one/docker-compose.prod.yml \
  up -d --no-deps --build api
```

### Zero-Downtime Deployment

1. Deploy new version alongside old
2. Health check new version
3. Switch traffic to new version
4. Remove old version

### Database Migrations

```bash
# Run migrations
alembic upgrade head

# Rollback if needed
alembic downgrade -1
```

## Troubleshooting

### High CPU Usage

```bash
# Check API processes
docker stats

# Scale up
docker-compose up -d --scale api=5
```

### Database Connection Pool Exhausted

```bash
# Increase pool size in .env
MATRIXONE_POOL_SIZE=50
MATRIXONE_MAX_OVERFLOW=20

# Restart API
docker-compose restart api
```

### Memory Leaks

```bash
# Monitor memory
docker stats

# Restart API periodically (temporary fix)
# Investigate and fix root cause
```

## Next Steps

- [Deployment Guide](../guides/deployment.md) - Advanced deployment scenarios
- [Configuration Reference](../reference/configuration.md) - All configuration options
- [Troubleshooting](../guides/troubleshooting.md) - Common issues
- [Monitoring Setup](../guides/monitoring.md) - Detailed monitoring guide
