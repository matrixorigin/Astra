# Deployment Guide

## Docker Compose (Recommended)

```bash
cd trustmem_cloud_v1
cp .env.example .env
# Edit .env — set TRUSTMEM_MASTER_KEY (min 16 chars)

docker compose up -d
```

> The build context is the project root (`context: ..` in docker-compose.yml) because TrustMem depends on `core/` and shared files in `api/`. Both `cd trustmem_cloud_v1 && docker compose up -d` and `docker compose -f trustmem_cloud_v1/docker-compose.yml up -d` work — Docker Compose resolves `context` relative to the compose file, not the working directory.

This starts three services:

| Service | Port | Description |
|---------|------|-------------|
| API | 8100 | TrustMem REST API (FastAPI + Uvicorn) |
| MatrixOne | 6001 | HTAP database (memory storage, vector search, snapshots) |
| Redis | 6379 | Rate limiting cache |

Verify:
```bash
curl http://localhost:8100/health
# {"status": "ok", "database": "connected"}
```

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `TRUSTMEM_MASTER_KEY` | **Yes** | — | Admin API key (min 16 chars) |
| `TRUSTMEM_DB_HOST` | No | `matrixone` | MatrixOne host |
| `TRUSTMEM_DB_PORT` | No | `6001` | MatrixOne port |
| `TRUSTMEM_DB_USER` | No | `root` | Database user |
| `TRUSTMEM_DB_PASSWORD` | No | `111` | Database password |
| `TRUSTMEM_DB_NAME` | No | `trustmem` | Database name |
| `TRUSTMEM_EMBEDDING_PROVIDER` | No | `local` | `local` or `openai` |
| `TRUSTMEM_EMBEDDING_MODEL` | No | `all-MiniLM-L6-v2` | Embedding model name |
| `TRUSTMEM_EMBEDDING_API_KEY` | No | — | Required if provider is `openai` |
| `TRUSTMEM_EMBEDDING_BASE_URL` | No | — | Custom embedding endpoint |
| `API_PORT` | No | `8100` | Host-side API port |

## External MatrixOne

To use an existing MatrixOne instance instead of the bundled one:

```bash
# .env
TRUSTMEM_DB_HOST=your-matrixone-host
TRUSTMEM_DB_PORT=6001
TRUSTMEM_DB_USER=root
TRUSTMEM_DB_PASSWORD=your-password
```

Start without the bundled DB:
```bash
docker compose up -d api redis
```

Tables are auto-created on first startup.

## Embedding Options

### Local (default)

No API key needed. Uses [sentence-transformers](https://www.sbert.net/) running in-process.

```bash
TRUSTMEM_EMBEDDING_PROVIDER=local
TRUSTMEM_EMBEDDING_MODEL=all-MiniLM-L6-v2
```

Build the image with local embedding support:
```bash
INSTALL_EXTRAS=local-embedding docker compose build
```

### OpenAI

```bash
TRUSTMEM_EMBEDDING_PROVIDER=openai
TRUSTMEM_EMBEDDING_MODEL=text-embedding-3-small
TRUSTMEM_EMBEDDING_API_KEY=sk-...
```

## Automated Governance

A background scheduler starts automatically with the API server:

| Frequency | Task |
|-----------|------|
| Hourly | Confidence decay for stale memories, quarantine low-quality entries |
| Daily | Clean up expired/quarantined memories |
| Weekly | Compress redundant memories |

No configuration needed. Admins can also trigger governance manually per user:

```bash
curl -X POST http://localhost:8100/admin/governance/alice/trigger \
  -H "Authorization: Bearer YOUR_MASTER_KEY"
```

## Security Notes

- API keys are SHA-256 hashed at rest — raw keys are never stored
- All queries are scoped to the authenticated user's `user_id`
- Master key is required for all admin operations
- Snapshot names are sanitized and regex-validated before entering SQL
- Rate limiting is per API key (in-memory sliding window; v2 will use Redis)
- Run behind a reverse proxy (nginx/Caddy) with TLS in production
