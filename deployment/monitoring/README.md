# Monitoring Stack

Prometheus + Grafana monitoring for mo-agent.

## Quick Start

```bash
# Start monitoring stack
docker-compose up -d

# Access services
open http://localhost:9091  # Prometheus
open http://localhost:3000  # Grafana (admin/admin)
```

## Components

### Prometheus

- **Port**: 9091
- **Config**: `prometheus.yml`
- **Data**: Stored in `prometheus-data` volume

**Metrics collected:**
- API request rate and latency
- Database connection pool
- LLM request metrics
- System metrics (CPU, memory, disk)

### Grafana

- **Port**: 3000
- **Default credentials**: admin/admin
- **Dashboards**: Pre-configured in `dashboards/`

**Available dashboards:**
- API Overview
- Database Performance
- System Metrics

### Node Exporter

- **Port**: 9100
- **Metrics**: System-level metrics (CPU, memory, disk, network)

## Configuration

### Add New Metrics Source

Edit `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'my-service'
    static_configs:
      - targets: ['host.docker.internal:9999']
```

### Create Custom Dashboard

1. Open Grafana (http://localhost:3000)
2. Click "+" → "Dashboard"
3. Add panels with PromQL queries
4. Save dashboard
5. Export JSON to `dashboards/`

## Metrics Reference

### API Metrics

```promql
# Request rate
rate(http_requests_total[5m])

# Response time (p95)
histogram_quantile(0.95, rate(http_request_duration_seconds_bucket[5m]))

# Error rate
rate(http_requests_total{status=~"5.."}[5m])
```

### Database Metrics

```promql
# Active connections
db_connections_active

# Query duration
rate(db_query_duration_seconds_sum[5m]) / rate(db_query_duration_seconds_count[5m])
```

### LLM Metrics

```promql
# LLM request rate
rate(llm_requests_total[5m])

# LLM cost
rate(llm_cost_total[5m])
```

## Alerting

### Configure Alertmanager

Create `alertmanager.yml`:

```yaml
global:
  smtp_smarthost: 'smtp.gmail.com:587'
  smtp_from: 'alerts@your-domain.com'
  smtp_auth_username: 'your-email@gmail.com'
  smtp_auth_password: 'your-password'

route:
  receiver: 'email'

receivers:
  - name: 'email'
    email_configs:
      - to: 'team@your-domain.com'
```

### Add to docker-compose.yml

```yaml
alertmanager:
  image: prom/alertmanager:latest
  ports:
    - "9093:9093"
  volumes:
    - ./alertmanager.yml:/etc/alertmanager/alertmanager.yml
```

### Create Alert Rules

Create `alerts.yml`:

```yaml
groups:
  - name: mo-agent
    rules:
      - alert: HighErrorRate
        expr: rate(http_requests_total{status=~"5.."}[5m]) > 0.05
        for: 5m
        labels:
          severity: critical
        annotations:
          summary: "High error rate detected"
          description: "Error rate is {{ $value }} (threshold: 0.05)"

      - alert: HighResponseTime
        expr: histogram_quantile(0.95, rate(http_request_duration_seconds_bucket[5m])) > 1
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "High response time"
          description: "P95 response time is {{ $value }}s"
```

## Troubleshooting

### Prometheus Not Scraping

```bash
# Check Prometheus targets
open http://localhost:9091/targets

# Check if API metrics endpoint is accessible
curl http://localhost:8000/metrics
```

### Grafana Dashboard Not Loading

```bash
# Check Grafana logs
docker logs grafana

# Verify datasource connection
# Grafana → Configuration → Data Sources → Prometheus → Test
```

### High Memory Usage

```bash
# Reduce retention period in prometheus.yml
--storage.tsdb.retention.time=7d

# Or limit memory
docker-compose up -d --scale prometheus=1 --memory=2g
```

## Production Recommendations

1. **Persistent Storage**: Use named volumes or bind mounts
2. **Backup**: Regularly backup Prometheus data
3. **Retention**: Set appropriate retention period (default: 15d)
4. **Alerting**: Configure Alertmanager for critical alerts
5. **Security**: Enable authentication and HTTPS
6. **High Availability**: Run multiple Prometheus instances

## See Also

- [Prometheus Documentation](https://prometheus.io/docs/)
- [Grafana Documentation](https://grafana.com/docs/)
- [PromQL Cheat Sheet](https://promlabs.com/promql-cheat-sheet/)
