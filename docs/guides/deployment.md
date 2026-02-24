# Deployment Guide

Complete guide for deploying mo-agent to various environments.

## Deployment Options

### 1. Docker Compose (Recommended for Small-Medium Scale)

**Best for:**
- Development and staging
- Small production deployments
- Single server setups
- Quick prototyping

**Pros:**
- Simple setup (one command)
- All services included
- Easy to debug
- Low resource requirements

**Cons:**
- Limited scalability
- No auto-scaling
- Single point of failure

**See:** [Docker Deployment Guide](../quickstart/docker.md)

### 2. Kubernetes (Recommended for Large Scale)

**Best for:**
- Production deployments
- High availability requirements
- Auto-scaling needs
- Enterprise environments

**Pros:**
- High availability
- Auto-scaling
- Rolling updates
- Resource management

**Cons:**
- Complex setup
- Higher resource requirements
- Steeper learning curve

**See:** [Kubernetes Deployment](#kubernetes-deployment)

### 3. Cloud Platforms

**Best for:**
- Managed infrastructure
- Quick production deployment
- Minimal ops overhead

**Options:**
- AWS (ECS, EKS, Lambda)
- GCP (Cloud Run, GKE)
- Azure (Container Instances, AKS)

**See:** [Cloud Platform Deployment](#cloud-platform-deployment)

---

## Docker Compose Deployment

### Quick Start

```bash
# 1. Clone repository
git clone https://github.com/matrixorigin/mo-agent.git
cd mo-agent

# 2. Configure
cp .env.example .env
# Edit .env with your settings

# 3. Start services
docker-compose -f deployment/all-in-one/docker-compose.yml up -d

# 4. Verify
curl http://localhost:8000/health
```

### Production Configuration

```bash
# Use production compose file
docker-compose -f deployment/all-in-one/docker-compose.prod.yml up -d

# Scale API servers
docker-compose -f deployment/all-in-one/docker-compose.prod.yml up -d --scale api=3
```

### Service Management

```bash
# View logs
docker-compose -f deployment/all-in-one/docker-compose.yml logs -f

# Restart services
docker-compose -f deployment/all-in-one/docker-compose.yml restart

# Stop services
docker-compose -f deployment/all-in-one/docker-compose.yml down

# Clean data
docker-compose -f deployment/all-in-one/docker-compose.yml down -v
```

**See:** [Docker Deployment Guide](../quickstart/docker.md) for details

---

## Kubernetes Deployment

### Prerequisites

- Kubernetes cluster (1.20+)
- kubectl configured
- Helm 3.x installed

### Quick Start

```bash
# 1. Add Helm repository (if published)
helm repo add mo-agent https://charts.mo-agent.io
helm repo update

# 2. Install
helm install mo-agent mo-agent/mo-agent \
  --namespace mo-agent \
  --create-namespace

# 3. Verify
kubectl get pods -n mo-agent
```

### From Source

```bash
# 1. Clone repository
git clone https://github.com/matrixorigin/mo-agent.git
cd mo-agent

# 2. Configure values
cp deployment/kubernetes/chart/values.yaml values.prod.yaml
# Edit values.prod.yaml

# 3. Install
helm install mo-agent deployment/kubernetes/chart \
  -f values.prod.yaml \
  --namespace mo-agent \
  --create-namespace

# 4. Verify
kubectl get pods -n mo-agent
kubectl get svc -n mo-agent
```

### Configuration

**values.yaml:**

```yaml
# API Server
api:
  replicaCount: 3
  image:
    repository: mo-agent/api
    tag: latest
  resources:
    limits:
      cpu: 2
      memory: 4Gi
    requests:
      cpu: 1
      memory: 2Gi
  autoscaling:
    enabled: true
    minReplicas: 2
    maxReplicas: 10
    targetCPUUtilizationPercentage: 70

# MatrixOne
matrixone:
  enabled: true
  replicaCount: 3
  persistence:
    enabled: true
    size: 100Gi

# Redis
redis:
  enabled: true
  cluster:
    enabled: true
    nodes: 3

# Ingress
ingress:
  enabled: true
  className: nginx
  hosts:
    - host: api.your-domain.com
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: api-tls
      hosts:
        - api.your-domain.com
```

### External Services

Use existing database and Redis:

```yaml
# values.yaml
matrixone:
  enabled: false
  external:
    host: matrixone-cluster.internal
    port: 6001
    user: mo_agent_user
    password: <password>
    database: mo_agent

redis:
  enabled: false
  external:
    host: redis-cluster.internal
    port: 6379
    password: <password>
```

### Service Management

```bash
# View pods
kubectl get pods -n mo-agent

# View logs
kubectl logs -f deployment/mo-agent-api -n mo-agent

# Scale API
kubectl scale deployment mo-agent-api --replicas=5 -n mo-agent

# Update
helm upgrade mo-agent deployment/kubernetes/chart \
  -f values.prod.yaml \
  --namespace mo-agent

# Rollback
helm rollback mo-agent -n mo-agent

# Uninstall
helm uninstall mo-agent -n mo-agent
```

**See:** [deployment/kubernetes/README.md](../../deployment/kubernetes/README.md) for details

---

## Cloud Platform Deployment

### AWS

#### ECS (Elastic Container Service)

```bash
# 1. Build and push image
docker build -t mo-agent:latest .
aws ecr get-login-password --region us-east-1 | docker login --username AWS --password-stdin <account>.dkr.ecr.us-east-1.amazonaws.com
docker tag mo-agent:latest <account>.dkr.ecr.us-east-1.amazonaws.com/mo-agent:latest
docker push <account>.dkr.ecr.us-east-1.amazonaws.com/mo-agent:latest

# 2. Create task definition
aws ecs register-task-definition --cli-input-json file://ecs-task-definition.json

# 3. Create service
aws ecs create-service --cluster mo-agent --service-name mo-agent-api --task-definition mo-agent --desired-count 3
```

#### EKS (Elastic Kubernetes Service)

```bash
# 1. Create EKS cluster
eksctl create cluster --name mo-agent --region us-east-1 --nodes 3

# 2. Deploy with Helm
helm install mo-agent deployment/kubernetes/chart \
  --namespace mo-agent \
  --create-namespace

# 3. Configure load balancer
kubectl apply -f deployment/aws/alb-ingress.yaml
```

### GCP

#### Cloud Run

```bash
# 1. Build and push image
gcloud builds submit --tag gcr.io/<project>/mo-agent

# 2. Deploy
gcloud run deploy mo-agent \
  --image gcr.io/<project>/mo-agent \
  --platform managed \
  --region us-central1 \
  --allow-unauthenticated
```

#### GKE (Google Kubernetes Engine)

```bash
# 1. Create GKE cluster
gcloud container clusters create mo-agent --num-nodes=3 --region=us-central1

# 2. Deploy with Helm
helm install mo-agent deployment/kubernetes/chart \
  --namespace mo-agent \
  --create-namespace
```

### Azure

#### Container Instances

```bash
# 1. Create resource group
az group create --name mo-agent --location eastus

# 2. Deploy container
az container create \
  --resource-group mo-agent \
  --name mo-agent-api \
  --image mo-agent:latest \
  --cpu 2 \
  --memory 4 \
  --ports 8000
```

#### AKS (Azure Kubernetes Service)

```bash
# 1. Create AKS cluster
az aks create --resource-group mo-agent --name mo-agent --node-count 3

# 2. Get credentials
az aks get-credentials --resource-group mo-agent --name mo-agent

# 3. Deploy with Helm
helm install mo-agent deployment/kubernetes/chart \
  --namespace mo-agent \
  --create-namespace
```

---

## Monitoring and Observability

### Prometheus + Grafana

```bash
# Deploy monitoring stack
docker-compose -f deployment/monitoring/docker-compose.yml up -d

# Access Grafana
open http://localhost:3000
# Default credentials: admin/admin
```

### Metrics

API exposes Prometheus metrics at `/metrics`:

```bash
curl http://localhost:8000/metrics
```

**Key metrics:**
- `http_requests_total` - Total HTTP requests
- `http_request_duration_seconds` - Request duration
- `db_connections_active` - Active database connections
- `llm_requests_total` - Total LLM requests
- `llm_request_duration_seconds` - LLM request duration

### Logging

Configure structured logging:

```bash
# .env
LOG_LEVEL=info
LOG_FORMAT=json
LOG_OUTPUT=/var/log/mo-agent/api.log
```

Aggregate logs with:
- ELK Stack (Elasticsearch, Logstash, Kibana)
- Loki + Grafana
- CloudWatch (AWS)
- Cloud Logging (GCP)

### Tracing

Enable distributed tracing:

```bash
# .env
ENABLE_TRACING=true
TRACING_ENDPOINT=http://jaeger:14268/api/traces
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
  mo_agent > backup-$(date +%Y%m%d).sql

# Upload to S3
aws s3 cp backup-$(date +%Y%m%d).sql s3://mo-agent-backups/
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

### Disaster Recovery Plan

1. **Daily automated backups** to off-site storage
2. **Weekly restore tests** to verify backups
3. **Documented failover procedures**
4. **Monitoring and alerting** for critical issues
5. **Incident response plan**

---

## Security

### Pre-Deployment Checklist

```bash
# Run security check
python scripts/check_security.py
```

**Verify:**
- ✅ Strong encryption keys
- ✅ No default passwords
- ✅ HTTPS enabled
- ✅ CORS properly configured
- ✅ Rate limiting enabled
- ✅ API keys not in code
- ✅ Database access restricted
- ✅ Secrets in environment variables

### SSL/TLS

**Nginx configuration:**

```nginx
server {
    listen 443 ssl http2;
    server_name api.your-domain.com;

    ssl_certificate /etc/ssl/certs/your-cert.pem;
    ssl_certificate_key /etc/ssl/private/your-key.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_ciphers HIGH:!aNULL:!MD5;

    location / {
        proxy_pass http://api:8000;
    }
}
```

### Secrets Management

Use secrets management service:
- AWS Secrets Manager
- GCP Secret Manager
- Azure Key Vault
- HashiCorp Vault

---

## Scaling

### Horizontal Scaling

```bash
# Docker Compose
docker-compose up -d --scale api=5

# Kubernetes
kubectl scale deployment mo-agent-api --replicas=5 -n mo-agent
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

### Load Balancing

Use load balancer:
- Nginx
- HAProxy
- AWS ALB
- GCP Load Balancer
- Azure Load Balancer

---

## See Also

- [Production Deployment](../quickstart/production.md) - Production setup
- [Docker Deployment](../quickstart/docker.md) - Docker guide
- [Configuration Reference](../reference/configuration.md) - All settings
- [Troubleshooting](troubleshooting.md) - Common issues
