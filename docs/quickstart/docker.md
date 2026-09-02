# Docker Deployment

Run Astra with the all-in-one Docker Compose stack.

## Quick Start

```bash
# 1. Clone repository
git clone <repo-url>
cd astra

# 2. Create stack environment
make stack-env
# Edit deployment/all-in-one/.env and fill:
#   MEMORIA_EMBEDDING_API_KEY
#   MEMORIA_EMBEDDING_BASE_URL

# 3. Start all services
make stack-up

# 4. Verify
curl http://localhost:17001/health
```

## Compose Stack

### All-in-One

Uses published images by default:

```bash
cd deployment/all-in-one
cp .env.example .env
# Edit required embedding configuration.

docker compose up -d
docker compose logs -f api

docker compose down
```

### API Docker Mode

For development, keep dependency services from `make dev-deps-up` and run only the API container:

```bash
# Build image
make dev-api-docker-build

# Start MatrixOne + Memoria deps
make dev-deps-up

# Start API container
make dev-api-docker-up

# Stop API container
make dev-api-docker-down
```

## Configuration

### Environment Variables

Create `deployment/all-in-one/.env` with:

```bash
# Required
MEMORIA_EMBEDDING_API_KEY=...
MEMORIA_EMBEDDING_BASE_URL=...

# Host ports
ASTRA_BIND_ADDRESS=127.0.0.1
ASTRA_API_PORT=17001
MEMORIA_PORT=8100
MATRIXONE_PORT=26001
MATRIXONE_DEBUG_HTTP_PORT=26060
```

Development ports bind to loopback by default. `ASTRA_BIND_ADDRESS` selects the
host interface, and `ASTRA_API_PORT` remaps the API port without changing the
in-container listener on `17001`.

See [Configuration Reference](../reference/configuration.md) for all options.

### Docker Compose Services

The stack includes:

- **api**: REST API (port 17001)
- **memoria**: memory service (port 8100)
- **matrixone**: database (port 26001, debug port 26060)

## Docker Commands

### Using Makefile

```bash
# Create stack environment
make stack-env

# Start stack
make stack-up

# Stop stack
make stack-down

# View logs
make stack-logs SERVICE=api
```

### Using Docker Compose Directly

```bash
cd deployment/all-in-one

# Start services
docker compose up -d

# View logs
docker compose logs -f api

# Stop services
docker compose down

# Remove volumes (clean data)
docker compose down -v
```

## Health Checks

### Check Service Status

```bash
# All services
make stack-status

# API health
curl http://localhost:17001/health

# MatrixOne
cd deployment/all-in-one && docker compose exec matrixone mysql -h127.0.0.1 -P6001 -uroot -p111 -e "SELECT 1"
```

## Troubleshooting

### Services Not Starting

```bash
# Check logs
make stack-logs

# Check specific service
make stack-logs SERVICE=api

# Restart services
cd deployment/all-in-one && docker compose restart
```

### Database Connection Issues

```bash
# Check MatrixOne status
cd deployment/all-in-one && docker compose ps matrixone

# View MatrixOne logs
cd deployment/all-in-one && docker compose logs matrixone

# Restart MatrixOne
cd deployment/all-in-one && docker compose restart matrixone
```

### Port Conflicts

If ports are already in use, modify `.env`:

```bash
ASTRA_API_PORT=8001
MATRIXONE_PORT=26002
MATRIXONE_DEBUG_HTTP_PORT=26061
```

### Clean Restart

```bash
# Stop and remove everything
cd deployment/all-in-one
docker compose down -v

# Start fresh
docker compose up -d
```

## Production Considerations

### Production Profile

Do not promote the development stack directly. The production Compose profile
uses published images, includes resource limits, and requires external
MatrixOne and Memoria services. See the [production guide](production.md).

### Persistent Data

Ensure data persistence with named volumes:

```yaml
volumes:
  matrixone-data:
    driver: local
```

### Monitoring

Add monitoring services (Prometheus, Grafana):

```bash
# See deployment/monitoring/ for configuration
cd deployment/monitoring && docker compose up -d
```

### Backup

```bash
# Backup MatrixOne data
cd deployment/all-in-one
docker compose exec -T matrixone mysqldump -h127.0.0.1 -P6001 -uroot -p111 astra_runtime > backup.sql

# Restore
docker compose exec -T matrixone mysql -h127.0.0.1 -P6001 -uroot -p111 astra_runtime < backup.sql
```

## Next Steps

- [Production Deployment](production.md) - Deploy to production
- [Configuration Reference](../reference/configuration.md) - All configuration options
- [Deployment Guide](../guides/deployment.md) - Advanced deployment scenarios
- [Troubleshooting](../guides/troubleshooting.md) - Common issues
