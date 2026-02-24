# Docker Deployment

Run mo-agent using Docker and Docker Compose.

## Quick Start

```bash
# 1. Clone repository
git clone https://github.com/matrixorigin/mo-agent.git
cd mo-agent

# 2. Configure environment
cp .env.example .env
# Edit .env with your settings

# 3. Start all services
make dev-start-docker

# 4. Verify
curl http://localhost:8000/health
```

## Docker Compose Modes

### Development Mode

Uses local source code with hot-reload:

```bash
# Start services
docker-compose -f deployment/all-in-one/docker-compose.yml up -d

# View logs
docker-compose -f deployment/all-in-one/docker-compose.yml logs -f

# Stop services
docker-compose -f deployment/all-in-one/docker-compose.yml down
```

### Production Mode

Uses pre-built Docker image:

```bash
# Build image
make dev-api-docker-build

# Start services
docker-compose -f deployment/all-in-one/docker-compose.prod.yml up -d

# Scale API servers
docker-compose -f deployment/all-in-one/docker-compose.prod.yml up -d --scale api=3
```

## Configuration

### Environment Variables

Create `.env` file with required variables:

```bash
# Security
TOKEN_ENCRYPTION_KEY=your-encryption-key-here
JWT_SECRET_KEY=your-jwt-secret-here

# LLM Configuration
LLM_PROVIDER=openai
LLM_MODEL=gpt-4
OPENAI_API_KEY=your-openai-key-here

# Database
MATRIXONE_HOST=matrixone
MATRIXONE_PORT=6001
MATRIXONE_USER=root
MATRIXONE_PASSWORD=111
MATRIXONE_DATABASE=mo_agent

# Redis
REDIS_HOST=redis
REDIS_PORT=6379

# API
API_PORT=8000
```

See [Configuration Reference](../reference/configuration.md) for all options.

### Docker Compose Services

The stack includes:

- **matrixone**: Database (port 6001)
- **redis**: Cache (port 6379)
- **api**: REST API (port 8000)

## Docker Commands

### Using Makefile

```bash
# Build API image
make dev-api-docker-build

# Start services
make dev-api-docker-up

# Stop services
make dev-api-docker-down

# View logs
make dev-api-docker-logs

# Scale API servers
make dev-api-docker-scale REPLICAS=3
```

### Using Docker Compose Directly

```bash
# Start services
docker-compose -f deployment/all-in-one/docker-compose.yml up -d

# View logs
docker-compose -f deployment/all-in-one/docker-compose.yml logs -f api

# Stop services
docker-compose -f deployment/all-in-one/docker-compose.yml down

# Remove volumes (clean data)
docker-compose -f deployment/all-in-one/docker-compose.yml down -v
```

## Health Checks

### Check Service Status

```bash
# All services
docker-compose -f deployment/all-in-one/docker-compose.yml ps

# API health
curl http://localhost:8000/health

# MatrixOne
docker exec -it matrixone mysql -h127.0.0.1 -P6001 -uroot -p111 -e "SELECT 1"

# Redis
docker exec -it redis redis-cli ping
```

## Troubleshooting

### Services Not Starting

```bash
# Check logs
docker-compose -f deployment/all-in-one/docker-compose.yml logs

# Check specific service
docker-compose -f deployment/all-in-one/docker-compose.yml logs api

# Restart services
docker-compose -f deployment/all-in-one/docker-compose.yml restart
```

### Database Connection Issues

```bash
# Check MatrixOne status
docker-compose -f deployment/all-in-one/docker-compose.yml ps matrixone

# View MatrixOne logs
docker-compose -f deployment/all-in-one/docker-compose.yml logs matrixone

# Restart MatrixOne
docker-compose -f deployment/all-in-one/docker-compose.yml restart matrixone
```

### Port Conflicts

If ports are already in use, modify `.env`:

```bash
API_PORT=8001
MATRIXONE_PORT=6002
REDIS_PORT=6380
```

### Clean Restart

```bash
# Stop and remove everything
docker-compose -f deployment/all-in-one/docker-compose.yml down -v

# Start fresh
docker-compose -f deployment/all-in-one/docker-compose.yml up -d
```

## Production Considerations

### Resource Limits

Add resource limits in `docker-compose.prod.yml`:

```yaml
services:
  api:
    deploy:
      resources:
        limits:
          cpus: '2'
          memory: 4G
        reservations:
          cpus: '1'
          memory: 2G
```

### Persistent Data

Ensure data persistence with named volumes:

```yaml
volumes:
  matrixone_data:
    driver: local
  redis_data:
    driver: local
```

### Monitoring

Add monitoring services (Prometheus, Grafana):

```bash
# See deployment/monitoring/ for configuration
docker-compose -f deployment/monitoring/docker-compose.yml up -d
```

### Backup

```bash
# Backup MatrixOne data
docker exec matrixone mysqldump -h127.0.0.1 -P6001 -uroot -p111 mo_agent > backup.sql

# Restore
docker exec -i matrixone mysql -h127.0.0.1 -P6001 -uroot -p111 mo_agent < backup.sql
```

## Next Steps

- [Production Deployment](production.md) - Deploy to production
- [Configuration Reference](../reference/configuration.md) - All configuration options
- [Deployment Guide](../guides/deployment.md) - Advanced deployment scenarios
- [Troubleshooting](../guides/troubleshooting.md) - Common issues
