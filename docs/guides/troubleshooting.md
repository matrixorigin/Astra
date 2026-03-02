# Troubleshooting Guide

Common issues and solutions for mo-agent development and deployment.

## Quick Diagnostics

```bash
# Check all services
make dev-status

# View logs
make dev-api-logs
make dev-deps-logs

# Health check
curl http://localhost:8000/health
```

---

## Services Won't Start

### MatrixOne Won't Start

**Symptoms:**
- `make dev-start` hangs
- `docker ps` shows MatrixOne as unhealthy
- Connection refused errors

**Solutions:**

```bash
# 1. Check MatrixOne logs
make dev-deps-logs

# 2. Check if port is in use
lsof -i :6001

# 3. Restart MatrixOne
make dev-deps-down
make dev-deps-up

# 4. Wait for MatrixOne (max 15 seconds)
make dev-deps-wait

# 5. If still failing, clean and restart
make dev-clean
make dev-start
```

**Common causes:**
- Port 6001 already in use
- Insufficient memory
- Corrupted data volume
- Docker daemon issues

### Redis Won't Start

**Symptoms:**
- Redis container not running
- Connection refused on port 6379

**Solutions:**

```bash
# 1. Check Redis logs
docker logs redis

# 2. Check if port is in use
lsof -i :6379

# 3. Restart Redis
docker restart redis

# 4. If failing, remove and recreate
docker rm -f redis
make dev-deps-up
```

### API Server Won't Start

**Symptoms:**
- `make dev-api-start` fails
- Port 8000 connection refused
- Import errors

**Solutions:**

```bash
# 1. Check API logs
make dev-api-logs

# 2. Check if port is in use
lsof -i :8000

# 3. Verify dependencies installed
pip list | grep fastapi

# 4. Check environment variables
cat .env

# 5. Restart API
make dev-api-restart

# 6. If failing, reinstall dependencies
make install-dev-deps
make dev-api-start
```

---

## Database Issues

### Connection Failed

**Error:** `Can't connect to MySQL server on 'localhost:6001'`

**Solutions:**

```bash
# 1. Check MatrixOne is running
docker ps | grep matrixone

# 2. Test connection manually
mysql -h127.0.0.1 -P6001 -uroot -p111

# 3. Check environment variables
echo $MATRIXONE_HOST
echo $MATRIXONE_PORT

# 4. Wait for MatrixOne to be ready
make dev-deps-wait

# 5. Restart MatrixOne
make dev-deps-down
make dev-deps-up
```

### Database Not Found

**Error:** `Unknown database 'mo_agent'`

**Solutions:**

```bash
# 1. Initialize database
make db-init

# 2. Or create manually
mysql -h127.0.0.1 -P6001 -uroot -p111 -e "CREATE DATABASE IF NOT EXISTS mo_agent"

# 3. Run migrations
make db-migrate
```

### Connection Pool Exhausted

**Error:** `QueuePool limit of size X overflow Y reached`

**Solutions:**

```bash
# 1. Increase pool size in .env
MATRIXONE_POOL_SIZE=20
MATRIXONE_MAX_OVERFLOW=10

# 2. Restart API
make dev-api-restart

# 3. Check for connection leaks in code
# Ensure all sessions are properly closed
```

### Slow Queries

**Symptoms:**
- API responses slow
- Database CPU high

**Solutions:**

```bash
# 1. Check slow queries
mysql -h127.0.0.1 -P6001 -uroot -p111 -e "SHOW PROCESSLIST"

# 2. Add indexes
# Review query patterns and add appropriate indexes

# 3. Optimize queries
# Use EXPLAIN to analyze query plans

# 4. Increase database resources
# Adjust Docker resource limits
```

---

## API Issues

### 401 Unauthorized

**Error:** `{"detail":"Not authenticated"}`

**Solutions:**

```bash
# 1. Register user
curl -X POST http://localhost:8000/auth/register \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"secret123","email":"alice@example.com"}'

# 2. Login to get token
curl -X POST http://localhost:8000/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username":"alice","password":"secret123"}'

# 3. Use token in requests
curl -X GET http://localhost:8000/auth/me \
  -H "Authorization: Bearer <your_token>"
```

### 500 Internal Server Error

**Solutions:**

```bash
# 1. Check API logs
make dev-api-logs

# 2. Check database connection
curl http://localhost:8000/health

# 3. Check environment variables
cat .env

# 4. Restart API
make dev-api-restart
```

### CORS Errors

**Error:** `Access to fetch at 'http://localhost:8000' from origin 'http://localhost:3000' has been blocked by CORS policy`

**Solutions:**

```bash
# 1. Add origin to .env
CORS_ORIGINS=http://localhost:3000,http://localhost:8000

# 2. Or allow all origins (development only)
CORS_ORIGINS=*

# 3. Restart API
make dev-api-restart
```

### Rate Limiting

**Error:** `429 Too Many Requests`

**Solutions:**

```bash
# 1. Increase rate limit in .env
RATE_LIMIT_PER_MINUTE=120

# 2. Or disable rate limiting (development only)
ENABLE_RATE_LIMITING=false

# 3. Restart API
make dev-api-restart
```

---

## Configuration Issues

### Missing Environment Variables

**Error:** `TOKEN_ENCRYPTION_KEY not set`

**Solutions:**

```bash
# 1. Run initialization (auto-generates keys)
make dev-init

# 2. Or manually create .env
cp .env.example .env
# Edit .env with your values

# 3. Verify configuration
cat .env | grep TOKEN_ENCRYPTION_KEY
```

### Invalid LLM Configuration

**Error:** `LLM provider 'openai' does not support model 'claude-3-opus'`

**Solutions:**

```bash
# 1. Fix model name in .env
LLM_PROVIDER=openai
LLM_MODEL=gpt-4

# Or change provider
LLM_PROVIDER=anthropic
LLM_MODEL=claude-3-opus

# 2. Run validation
make dev-init

# 3. Restart API
make dev-api-restart
```

### Missing API Keys

**Error:** `OPENAI_API_KEY not set`

**Solutions:**

```bash
# 1. Add API key to .env
OPENAI_API_KEY=sk-...

# 2. Or use environment variable
export OPENAI_API_KEY=sk-...

# 3. Restart API
make dev-api-restart
```

---

## Docker Issues

### Port Conflicts

**Error:** `Bind for 0.0.0.0:6001 failed: port is already allocated`

**Solutions:**

```bash
# 1. Find process using port
lsof -i :6001

# 2. Kill process
kill -9 <PID>

# 3. Or change port in .env
MATRIXONE_PORT=6002

# 4. Restart services
make dev-deps-down
make dev-deps-up
```

### Out of Disk Space

**Error:** `no space left on device`

**Solutions:**

```bash
# 1. Check disk usage
df -h

# 2. Clean Docker
docker system prune -a

# 3. Remove unused volumes
docker volume prune

# 4. Clean mo-agent data
make dev-clean
```

### Container Won't Stop

**Symptoms:**
- `make dev-stop` hangs
- Container still running after stop

**Solutions:**

```bash
# 1. Force stop
docker stop -t 1 matrixone redis

# 2. Force kill
docker kill matrixone redis

# 3. Remove containers
docker rm -f matrixone redis

# 4. Restart
make dev-start
```

---

## Testing Issues

### Tests Failing

**Solutions:**

```bash
# 1. Ensure services are running
make dev-status

# 2. Run tests with verbose output
pytest -vv

# 3. Run specific failing test
pytest tests/unit/test_auth.py::test_login -vv

# 4. Check test database
mysql -h127.0.0.1 -P6001 -uroot -p111 -e "SHOW DATABASES"

# 5. Reset test environment
make dev-clean
make dev-start
make dev-test-keep
```

### Import Errors in Tests

**Error:** `ModuleNotFoundError: No module named 'core'`

**Solutions:**

```bash
# 1. Install package in development mode
pip install -e .

# 2. Verify installation
pip list | grep mo-agent

# 3. Check PYTHONPATH
echo $PYTHONPATH

# 4. Run tests from project root
cd /path/to/mo-agent
pytest
```

---

## Performance Issues

### High CPU Usage

**Solutions:**

```bash
# 1. Check processes
docker stats

# 2. Scale API servers
make dev-api-docker-scale REPLICAS=3

# 3. Optimize queries
# Add indexes, use query caching

# 4. Increase resources
# Adjust Docker resource limits
```

### High Memory Usage

**Solutions:**

```bash
# 1. Check memory usage
docker stats

# 2. Reduce connection pool size
MATRIXONE_POOL_SIZE=5
REDIS_POOL_SIZE=10

# 3. Restart services
make dev-restart

# 4. Check for memory leaks
# Profile application with memory_profiler
```

### Slow API Responses

**Solutions:**

```bash
# 1. Check database performance
# Add indexes, optimize queries

# 2. Enable caching
# Use Redis for frequently accessed data

# 3. Scale horizontally
make dev-api-docker-scale REPLICAS=3

# 4. Profile slow endpoints
# Use FastAPI profiling middleware
```

---

## Development Issues

### Code Changes Not Reflected

**Solutions:**

```bash
# 1. Restart API (auto-reload should work)
make dev-api-restart

# 2. Check if auto-reload is enabled
grep API_RELOAD .env

# 3. Force restart
make dev-api-stop
make dev-api-start

# 4. Clear Python cache
find . -type d -name __pycache__ -exec rm -rf {} +
```

### Import Errors

**Error:** `ModuleNotFoundError: No module named 'X'`

**Solutions:**

```bash
# 1. Reinstall dependencies
make install-dev-deps

# 2. Install specific package
pip install <package>

# 3. Check virtual environment
which python
pip list

# 4. Activate correct environment
conda activate dev-agent
```

---

## Getting Help

### Collect Diagnostic Information

```bash
# 1. Service status
make dev-status > diagnostics.txt

# 2. Logs
make dev-api-logs >> diagnostics.txt
make dev-deps-logs >> diagnostics.txt

# 3. Configuration
cat .env >> diagnostics.txt

# 4. Environment
python --version >> diagnostics.txt
docker --version >> diagnostics.txt
```

### Report Issues

When reporting issues, include:
- Error message
- Steps to reproduce
- Service status (`make dev-status`)
- Relevant logs
- Environment (OS, Python version, Docker version)

### Resources

- [Development Workflow](development-workflow.md) - Development guide
- [Configuration Reference](../reference/configuration.md) - All settings
- [API Reference](../reference/api-reference.md) - API documentation
- [GitHub Issues](https://github.com/matrixorigin/mo-agent/issues) - Report bugs

---

## Prevention

### Best Practices

1. **Always run `make dev-init` after cloning**
2. **Use `make dev-status` to check services before debugging**
3. **Keep dependencies updated**: `pip install --upgrade -e .`
4. **Clean data periodically**: `make dev-clean`
5. **Monitor logs**: `make dev-api-logs` and `make dev-deps-logs`
6. **Run tests before committing**: `make dev-test-keep`
7. **Use version control**: Commit working configurations

### Health Checks

```bash
# Daily health check
make dev-status
curl http://localhost:8000/health

# Weekly cleanup
make dev-clean
make dev-start

# Monthly updates
pip install --upgrade -e .
docker pull matrixorigin/matrixone:latest
docker pull redis:7-alpine
```
