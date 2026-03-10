# TrustMem Cloud v1

Multi-tenant memory service for AI assistants. Powered by [MatrixOne](https://github.com/matrixorigin/matrixone).

TrustMem gives your AI assistant persistent, per-user memory — store facts, retrieve context, take snapshots, and run automated memory governance. It works as a standalone backend service; user management is delegated to your upstream system.

```
┌──────────────────────────────────────────────────┐
│  Your Application / AI Assistant                 │
│  (Claude, Cursor, custom app, SaaS platform)     │
└──────────┬──────────────────────┬────────────────┘
           │ MCP (stdio)          │ REST API
           ▼                      ▼
┌──────────────────────────────────────────────────┐
│  TrustMem Cloud v1                               │
│  ┌────────────┐  ┌───────────┐  ┌────────────┐  │
│  │ API Server │  │ MCP Server│  │ Governance  │  │
│  │ (FastAPI)  │  │ (stdio)   │  │ Scheduler   │  │
│  └─────┬──────┘  └─────┬─────┘  └──────┬─────┘  │
│        └────────────────┴───────────────┘        │
│                         │                        │
│  ┌──────────────────────▼──────────────────────┐ │
│  │  MatrixOne (HTAP Database)                  │ │
│  │  • Memory storage + vector search           │ │
│  │  • Native snapshots (time-travel)           │ │
│  │  • Fulltext search                          │ │
│  └─────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────┘
```

## Quick Start

```bash
cd trustmem_cloud_v1
cp .env.example .env        # Set TRUSTMEM_MASTER_KEY (min 16 chars)
docker compose up -d        # Start on port 8100
```

```bash
# Create a user + API key
curl -X POST http://localhost:8100/auth/keys \
  -H "Authorization: Bearer YOUR_MASTER_KEY" \
  -H "Content-Type: application/json" \
  -d '{"user_id": "alice", "name": "alice-dev"}'

# Store a memory
curl -X POST http://localhost:8100/v1/memories \
  -H "Authorization: Bearer sk-abc123..." \
  -H "Content-Type: application/json" \
  -d '{"content": "User prefers Python over JavaScript"}'

# Retrieve
curl -X POST http://localhost:8100/v1/memories/retrieve \
  -H "Authorization: Bearer sk-abc123..." \
  -H "Content-Type: application/json" \
  -d '{"query": "programming language preference"}'
```

## Documentation

| Document | Audience | Content |
|----------|----------|---------|
| [Deployment Guide](docs/deployment.md) | Ops / Admin | Docker setup, environment variables, embedding options, external DB |
| [User Guide](docs/user-guide.md) | Developers / Integrators | MCP setup, REST API examples, enterprise integration patterns |
| [API Reference](docs/api-reference.md) | Developers | Complete endpoint list, request/response schemas, rate limits |

## Key Features

- **Vector + Fulltext Retrieval** — hybrid search combining semantic similarity and keyword matching
- **Native Snapshots** — read-only point-in-time snapshots with diff comparison (powered by MatrixOne time-travel)
- **Automated Governance** — background scheduler handles confidence decay, quarantine, and compression
- **Multi-tenant Isolation** — every query scoped to authenticated user's `user_id`
- **Two Access Modes** — MCP for AI assistants, REST API for applications
- **Headless Design** — no user registration UI; user lifecycle managed by admin API, ideal for enterprise integration
