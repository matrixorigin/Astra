# Monitoring Stack

Prometheus + Grafana monitoring for astra.

## Quick Start

```bash
cp .env.example .env
# Set GRAFANA_ADMIN_PASSWORD in .env to a strong, unique value.
docker compose up -d

# Access services
open http://localhost:9091  # Prometheus
open http://localhost:3000  # Grafana
```

## Components

### Prometheus

- **Port**: 9091
- **Default bind address**: `127.0.0.1`
- **Config**: `prometheus.yml`
- **Data**: Stored in `prometheus-data` volume

**Metrics collected:**

- Astra runtime capacity and admission
- run control, durable event ingestion, and Edge dispatch health
- LLM provider admission and rate-limit state
- host CPU, memory, disk, and network metrics through Node Exporter

Prometheus loads the repository's `monitoring/alert-rules.yml` automatically.
Those rules use metric names exported by Astra's `/metrics` endpoint.

### Grafana

- **Port**: 3000
- **Default bind address**: `127.0.0.1`
- **Credentials**: `GRAFANA_ADMIN_USER` and `GRAFANA_ADMIN_PASSWORD` from `.env`
- **Dashboards**: Pre-configured in `dashboards/`

**Available dashboards:**

- API Overview
- Runtime Capacity

### Node Exporter

- **Network access**: internal to the monitoring Compose network
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

### Runtime Metrics

```promql
# Run admission rate
rate(astra_run_admission_attempts_total[5m])

# Event ingestion errors
rate(astra_event_ingestion_errors_total[5m])

# Edge dispatch backlog
astra_edge_dispatch_pending_rows
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

The bundled alert rules are evaluated by Prometheus. To route notifications,
add Alertmanager and configure Prometheus with its endpoint; Prometheus alone
records alert state but does not send notifications.

## Troubleshooting

### Prometheus Not Scraping

```bash
# Check Prometheus targets
open http://localhost:9091/targets

# Check if API metrics endpoint is accessible
curl http://localhost:17001/metrics
```

### Grafana Dashboard Not Loading

```bash
# Check Grafana logs
docker compose logs grafana

# Verify datasource connection
# Grafana → Configuration → Data Sources → Prometheus → Test
```

### High Memory Usage

```bash
# Reduce retention period in prometheus.yml
--storage.tsdb.retention.time=7d

# Or limit memory
docker compose up -d --scale prometheus=1 --memory=2g
```

## Production Recommendations

1. **Persistent Storage**: Use named volumes or bind mounts
2. **Backup**: Regularly backup Prometheus data
3. **Retention**: Set appropriate retention period (default: 15d)
4. **Alerting**: Configure Alertmanager for critical alerts
5. **Security**: Keep the default loopback bindings, or place Prometheus and
   Grafana behind an authenticated HTTPS reverse proxy before exposing them
6. **High Availability**: Run multiple Prometheus instances

## See Also

- [Prometheus Documentation](https://prometheus.io/docs/)
- [Grafana Documentation](https://grafana.com/docs/)
- [PromQL Cheat Sheet](https://promlabs.com/promql-cheat-sheet/)
