---
inclusion: always
---

# TrustMem Cloud v1 — Developer Guide

## What Is TrustMem Cloud v1

TrustMem Cloud v1 is a **multi-tenant memory service** living in `trustmem_cloud_v1/`. It is a standalone FastAPI application that provides persistent, per-user memory for AI assistants and applications.

It is **not** the main mo-agent-engine API. It shares `core/memory/` logic but has its own:
- Database tables (`tm_users`, `auth_api_keys`, `mem_snapshot_registry`)
- Auth model (API key only, no JWT)
- Entry point (`trustmem_cloud_v1/api/main.py`)
- Docker deployment (`trustmem_cloud_v1/Dockerfile`, `trustmem_cloud_v1/docker-compose.yml`)

## Directory Layout

```
trustmem_cloud_v1/
├── api/
│   ├── main.py              # FastAPI app, lifespan (embedding init + governance scheduler)
│   ├── database.py          # MatrixOne engine, session factory, table init
│   ├── dependencies.py      # get_current_user_id, require_admin
│   ├── middleware.py        # Rate limiting (per API key, sliding window)
│   ├── models.py            # ORM: User, ApiKey, SnapshotRegistry
│   └── routers/
│       ├── auth.py          # POST/GET/DELETE /auth/keys
│       ├── memory.py        # /v1/memories/* + /v1/profiles/* + /v1/observe
│       ├── snapshots.py     # /v1/snapshots/* (MatrixOne native snapshots)
│       ├── user_ops.py      # POST /v1/consolidate, /v1/reflect
│       ├── admin.py         # /admin/* (master key required)
│       └── health.py        # GET /health
├── mcp/
│   └── server.py            # MCP stdio server — proxies to REST API
├── tests/
│   ├── test_e2e.py          # TestClient tests with DB ground truth (94 tests)
│   └── test_docker.py       # httpx tests against live Docker container (55 tests)
├── docs/
│   ├── deployment.md        # Docker setup, env vars, embedding options
│   ├── user-guide.md        # API key, MCP config, REST examples, enterprise integration
│   └── api-reference.md     # All 26 endpoints with request/response schemas
├── Dockerfile               # Build context: project root (needs core/, api/ shared files)
├── docker-compose.yml       # MatrixOne + API, isolated network trustmem-net, bind mount data
├── .env                     # Local dev config (pre-configured, not committed)
├── .env.example             # Environment variable template
├── config.py                # TrustMemSettings (TRUSTMEM_* env vars)
└── README.md                # Overview + quick start + doc links
```

## Key Design Decisions

### Headless User Management
No user registration UI. Users are created by admin via `POST /auth/keys`. This is intentional — TrustMem is designed to be embedded in existing platforms that already have user identity.

### Auth Model
- Users authenticate with API keys (`Authorization: Bearer sk-...`)
- Admin operations require the master key (`TRUSTMEM_MASTER_KEY`)
- API keys are SHA-256 hashed at rest — raw key shown only at creation
- All queries are automatically scoped to the `user_id` derived from the API key

### MatrixOne `is_active` Bug
**Critical**: `is_active = 1` in compound WHERE clauses (with `AND user_id = :uid`) returns 0 rows in MatrixOne despite the value being 1. Always use `is_active` (boolean truthy) instead of `is_active = 1`.

```python
# ❌ Wrong — returns 0 rows in MatrixOne with compound WHERE
db.query(M).filter(M.user_id == uid, M.is_active == 1)

# ✅ Correct
db.query(M).filter(M.user_id == uid, M.is_active)
```

### Snapshot Queries
Snapshots use MatrixOne's native time-travel via the `matrixone.sqlalchemy_ext.snapshot` SDK:

```python
from matrixone.sqlalchemy_ext.snapshot import select as mo_select, compile_select

stmt = mo_select(M).where(M.user_id == uid).with_snapshot(snapshot_name)
result = db.execute(text(compile_select(stmt)))
```

Cross-snapshot LEFT JOIN queries (for diff) use raw SQL — the SDK only handles single-table snapshot injection.

### Snapshot Name Sanitization
Snapshot names go into SQL. Defense-in-depth:
1. `_sanitize()` replaces non-alphanumeric chars with `_`
2. `re.fullmatch(r"[a-zA-Z0-9_]+", sn)` validates the final name

### Rate Limiting
In-memory sliding window per API key. Configurable via env vars:

```bash
TRUSTMEM_RATE_LIMIT_AUTH_KEYS=1000,60    # for testing
TRUSTMEM_RATE_LIMIT_CONSOLIDATE=100,60   # for testing
```

Format: `max_requests,window_seconds`. See `middleware.py` for all configurable keys.

## Running Tests

```bash
# Unit/integration tests (TestClient + real DB)
python -m pytest trustmem_cloud_v1/tests/test_e2e.py -v

# Docker integration tests (requires running container)
cd trustmem_cloud_v1 && docker compose up -d
python -m pytest trustmem_cloud_v1/tests/test_docker.py -v

# All tests
python -m pytest trustmem_cloud_v1/tests/ -v
```

For Docker tests, set relaxed rate limits in `.env`:
```bash
TRUSTMEM_RATE_LIMIT_AUTH_KEYS=1000,60
TRUSTMEM_RATE_LIMIT_CONSOLIDATE=100,60
TRUSTMEM_RATE_LIMIT_REFLECT=100,60
```

## Local Development

TrustMem reuses the main project's MatrixOne instance. Just set env vars and run:

```bash
export TRUSTMEM_MASTER_KEY=dev-master-key-1234
export TRUSTMEM_DB_NAME=trustmem_dev   # separate DB from main project
uvicorn trustmem_cloud_v1.api.main:app --reload --port 8100
```

## Docker Deployment

A `.env` is pre-configured for local dev. Just run:

```bash
cd trustmem_cloud_v1
docker compose up -d
```

For a fresh environment:
```bash
cp .env.example .env   # fill TRUSTMEM_MASTER_KEY + embedding config
docker compose up -d
```

Build context is the project root (`context: ..`) because the image needs `core/` and shared `api/` files. Both `cd trustmem_cloud_v1 && docker compose up -d` and `docker compose -f trustmem_cloud_v1/docker-compose.yml up -d` work.

Data is bind-mounted to `./data/matrixone` — survives restarts and `docker compose down`.

## Adding New Endpoints

1. Add router in `trustmem_cloud_v1/api/routers/`
2. Register in `main.py` with `app.include_router(..., prefix="/v1")`
3. Add rate limit entry in `middleware.py` `_RATE_LIMITS`
4. Add tests in `test_e2e.py` (with DB ground truth) and `test_docker.py` (HTTP only)
5. Update `docs/api-reference.md`

## Common Pitfalls

| Symptom | Fix |
|---------|-----|
| Query returns 0 rows with `is_active = 1` | Use `is_active` (boolean truthy) |
| `ModuleNotFoundError: No module named 'skills'` in Docker | `core/context/__init__.py` imports `skills` — Dockerfile overwrites it with empty init |
| `ModuleNotFoundError: No module named 'config.settings'` in Docker | Add `COPY config/ config/` to Dockerfile |
| Rate limit 429 in tests | Set `TRUSTMEM_RATE_LIMIT_AUTH_KEYS=1000,60` in `.env` |
| `http_proxy` breaks localhost requests | Use `curl --noproxy localhost` or `httpx.Client(trust_env=False)` |
| Snapshot healthcheck fails in Docker | MatrixOne image has no `mysql` client — use `bash /dev/tcp` TCP check |
| Data lost after `docker compose down -v` | Use `docker compose down` (no `-v`) — data is in bind mount `./data/matrixone` |
