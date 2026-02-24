# On-Premise Deployment

Deploy mo-agent to on-premise infrastructure.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                  On-Premise Infrastructure               │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │   Nginx    │─────▶│   API Servers (3x)       │      │
│  │ (HAProxy)  │      │   Docker / Systemd       │      │
│  └────────────┘      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ MatrixOne  │      │        Redis             │      │
│  │  Cluster   │      │       Cluster            │      │
│  └────────────┘      └──────────────────────────┘      │
│                                                          │
│  ┌────────────┐      ┌──────────────────────────┐      │
│  │ Prometheus │      │       Grafana            │      │
│  │  + Alertmgr│      │                          │      │
│  └────────────┘      └──────────────────────────┘      │
└─────────────────────────────────────────────────────────┘
```

## Prerequisites

- Linux servers (Ubuntu 20.04+ or CentOS 7+)
- Docker and Docker Compose installed
- Network connectivity between servers
- SSL certificates (for HTTPS)

## Deployment Options

### Option 1: Docker Compose (Recommended)

```bash
# 1. Copy files to server
scp -r deployment/all-in-one user@server:/opt/mo-agent/

# 2. Configure environment
ssh user@server
cd /opt/mo-agent
cp .env.example .env
# Edit .env with your settings

# 3. Start services
docker-compose -f docker-compose.prod.yml up -d

# 4. Verify
curl http://localhost:8000/health
```

### Option 2: Systemd Services

```bash
# 1. Install dependencies
sudo apt-get update
sudo apt-get install python3.11 python3-pip

# 2. Install mo-agent
cd /opt/mo-agent
pip3 install -e .

# 3. Create systemd service
sudo cp deployment/examples/on-premise/mo-agent.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable mo-agent
sudo systemctl start mo-agent

# 4. Check status
sudo systemctl status mo-agent
```

## Components

### 1. Load Balancer (Nginx)

```nginx
# /etc/nginx/sites-available/mo-agent
upstream api_backend {
    least_conn;
    server 10.0.1.10:8000 max_fails=3 fail_timeout=30s;
    server 10.0.1.11:8000 max_fails=3 fail_timeout=30s;
    server 10.0.1.12:8000 max_fails=3 fail_timeout=30s;
}

server {
    listen 443 ssl http2;
    server_name api.your-domain.com;

    ssl_certificate /etc/ssl/certs/your-cert.pem;
    ssl_certificate_key /etc/ssl/private/your-key.pem;

    location / {
        proxy_pass http://api_backend;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### 2. Database (MatrixOne Cluster)

```bash
# Server 1 (Primary)
docker run -d \
  --name matrixone \
  -p 6001:6001 \
  -v /data/matrixone:/mo-data \
  matrixorigin/matrixone:latest

# Server 2, 3 (Replicas)
# Configure replication as needed
```

### 3. Cache (Redis Cluster)

```bash
# Create Redis cluster
docker run -d \
  --name redis \
  -p 6379:6379 \
  -v /data/redis:/data \
  redis:7.2-alpine redis-server --appendonly yes
```

### 4. Monitoring

```bash
# Deploy monitoring stack
cd deployment/monitoring
docker-compose up -d

# Access Grafana
open http://monitoring-server:3000
```

## High Availability

### Database Replication

```bash
# Configure MatrixOne replication
# See MatrixOne documentation for cluster setup
```

### Redis Sentinel

```bash
# Deploy Redis Sentinel for automatic failover
docker run -d \
  --name redis-sentinel \
  -p 26379:26379 \
  redis:7.2-alpine redis-sentinel /etc/redis/sentinel.conf
```

### API Server Redundancy

- Deploy at least 3 API servers
- Use load balancer health checks
- Configure automatic restart on failure

## Backup

```bash
# Automated daily backup
crontab -e

# Add:
0 2 * * * /opt/mo-agent/scripts/ops/backup.sh
```

## Monitoring

### Prometheus Alerts

```yaml
# /etc/prometheus/alerts.yml
groups:
  - name: mo-agent
    rules:
      - alert: HighErrorRate
        expr: rate(http_requests_total{status=~"5.."}[5m]) > 0.05
        for: 5m
        annotations:
          summary: "High error rate detected"
```

### Log Aggregation

```bash
# Use ELK stack or similar
docker run -d \
  --name elasticsearch \
  -p 9200:9200 \
  elasticsearch:8.0.0

docker run -d \
  --name kibana \
  -p 5601:5601 \
  kibana:8.0.0
```

## Security

### Firewall Rules

```bash
# Allow only necessary ports
sudo ufw allow 443/tcp  # HTTPS
sudo ufw allow 22/tcp   # SSH
sudo ufw enable
```

### SSL/TLS

```bash
# Use Let's Encrypt
sudo certbot --nginx -d api.your-domain.com
```

### Secrets Management

```bash
# Use HashiCorp Vault or similar
vault kv put secret/mo-agent \
  token_key="..." \
  jwt_secret="..."
```

## Maintenance

### Updates

```bash
# Pull latest image
docker pull mo-agent:latest

# Rolling update
docker-compose -f docker-compose.prod.yml up -d --no-deps --build api
```

### Health Checks

```bash
# Automated health check
*/5 * * * * /opt/mo-agent/scripts/ops/health_check.sh || /usr/bin/alert-admin
```

## See Also

- [mo-agent.service](mo-agent.service) - Systemd service file
- [nginx.conf](nginx.conf) - Nginx configuration
- [backup-cron.sh](backup-cron.sh) - Automated backup script
