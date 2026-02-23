# Unified Implementation Plan

> **Last Updated**: 2026-02-23
> **Scope**: Two workstreams — Write Path Optimization + CLI SaaS Architecture
> **Design Docs**: [write-path-optimization.md](write-path-optimization.md), [deployment-architecture.md](deployment-architecture.md) §1.1

---

## Overview

Two independent workstreams that can be developed in parallel. No dependency between them until the final integration phase.

```
Workstream A: Write Path (性能)          Workstream B: CLI Architecture (SaaS)
─────────────────────────────           ──────────────────────────────────────
A1: EventPipeline core                  B1: API client module
A2: Wire into ChatLoop/RunEngine        B2: Admin API endpoints
A3: Embedding decoupling                B3: Migrate mo-agent → API
A4: Async snapshot + firewall           B4: Migrate mo-admin → API
A5: Replay migration                    B5: Remove direct DB path
         │                                        │
         └──────────── Integration ───────────────┘
              EventPipeline + API-based CLI
```

---

## Workstream A: Write Path Optimization

**Goal**: 60x hot-path latency reduction (1.8s → ~30ms per turn).
**Design doc**: [write-path-optimization.md](write-path-optimization.md)

### A1: EventPipeline core

**New file**: `core/events/pipeline.py`

| Item | Detail |
|---|---|
| `EventPipeline` class | `emit()`, `flush_critical()`, `_flush_loop()`, `shutdown()` |
| Event classification | `CRITICAL_TYPES`, `DURABLE_TYPES`, everything else = ephemeral |
| Routing | `conversation_events` for critical+durable; `run_events` for any event with `run_id`. **Dual write is expected**: a critical event with `run_id` writes to both tables. The two routing dimensions (tier → conversation_events, run_id → run_events) are orthogonal. |
| Flush loop | asyncio background task, drain every 200ms or 50 events |
| Bulk INSERT | Single `INSERT ... VALUES` per table per batch, single `COMMIT` |
| Shutdown | `atexit` + signal handlers, 2s drain deadline, best-effort final flush |
| Backpressure | Warn at 10K queued, drop ephemeral at 100K |

**Validation**:
- Unit test: emit 1000 events → all flushed within 1s
- Unit test: `flush_critical()` commits immediately, returns < 50ms
- Unit test: graceful shutdown drains queue

**Rollback**: Additive. Old `EventLogger` untouched.

---

### A2: Wire into ChatLoop + RunEngine

**Modified files**: `core/agent/chat_loop.py`, `core/agent/run_engine.py`, `api/routers/chat.py`

| Item | Detail |
|---|---|
| ChatLoop | Replace `event_logger.create_*` calls with `pipeline.emit()` |
| RunEngine | Replace `_append_event` DB writes with `pipeline.emit()` |
| Sync points | `flush_critical()` after `user_query`; `flush_critical()` after `run_completed/failed/cancelled` |
| Feature flag | `EVENT_PIPELINE_ENABLED` env var, default `true` |
| EventLogger | Keep as facade — delegates to pipeline internally |

**Validation**:
- Integration test: 10 chat turns, assert hot-path write < 50ms each (p95)
- Integration test: events appear in DB within 300ms
- Regression: existing test suite passes unchanged

**Rollback**: `EVENT_PIPELINE_ENABLED=false` → synchronous writes.

---

### A3: Embedding decoupling

**New file**: `core/events/embedding_worker.py`
**Modified files**: `core/events/event_logger.py`, `core/context/hybrid_retrieval.py`, `api/models.py`

| Item | Detail |
|---|---|
| Remove embedding from EventLogger | `log_event()` no longer calls `EmbeddingService` |
| EmbeddingWorker | Async task: poll `conversation_events` LEFT JOIN `event_embeddings` WHERE embedding IS NULL, generate, INSERT into `event_embeddings` |
| Embed types | Only `user_query`, `llm_response`, `plan_created`, `knowledge_extracted` |
| hybrid_retrieval.py | `JOIN event_embeddings` instead of `WHERE e.embedding IS NOT NULL` |
| Fulltext fallback | Must return meaningful results when zero embeddings available |
| Migration script | Copy `conversation_events.embedding` → `event_embeddings` |
| DDL | `ALTER TABLE conversation_events DROP COLUMN embedding` — **only after all three conditions are met**: (1) migration script verified with row count match, (2) `hybrid_retrieval.py` running on JOIN path for ≥7 days with no retrieval regression, (3) rollback config flag removed from codebase. Until then, the column remains (nullable, not written to). |

**Validation**:
- Integration test: `event_embeddings` rows appear within 500ms of event commit
- Integration test: hybrid retrieval returns results with zero embeddings (fulltext-only)
- Migration test: verify row count match after copy

**Rollback**: Keep `conversation_events.embedding` column as long as needed. `hybrid_retrieval.py` reads from `event_embeddings` JOIN by default; config flag `EMBEDDING_SOURCE=legacy` switches back to reading `conversation_events.embedding`. The column is safe to drop only when the flag is removed.

---

### A4: Async snapshot + firewall

**Modified files**: `core/context/manager.py`, `core/verification/firewall.py`

| Item | Detail |
|---|---|
| `save_snapshot` | Assign snapshot_id synchronously, emit content write to pipeline |
| `log_verification` | Emit hallucination_checks + claim_evidence writes to pipeline |

**Validation**: Existing snapshot/firewall tests pass. Snapshot content visible within 300ms.

---

### A5: Replay migration

**Modified files**: `core/agent/stream_replay.py`

| Item | Detail |
|---|---|
| Primary path | Read `llm_response` from `conversation_events` (full-text replay) |
| Chunk-level path | Read `stream_text_delta` from `run_events`, gated on run completion |
| Fallback | If chunks missing → degrade to full-text, log warning |
| Cross-worker | Wait for `run_completed/failed` event before reading `run_events` |

**⚠️ Highest-risk refactor.** Required tests:
- Replay normal completed run → chunk-level output matches original
- Replay after simulated crash (missing chunks) → graceful fallback to full-text
- Replay from different worker → correct wait-for-completion
- Replay of tool-only turn (zero stream events) → no crash
- Regression: compare replay output before/after for 100 historical runs

---

## Workstream B: CLI SaaS Architecture

**Goal**: CLI → API → DB. No DB credentials on client.
**Design doc**: [deployment-architecture.md](deployment-architecture.md) §1.1

### B1: API client module

**New file**: `cli/api_client.py`

| Item | Detail |
|---|---|
| `APIClient` class | Base URL config, `httpx.AsyncClient` |
| Auth | JWT storage in `~/.mo-agent/credentials.json`, auto-refresh on 401 |
| Methods | Typed methods for each endpoint: `chat_stream()`, `list_sessions()`, `list_skills()`, etc. |
| SSE | `httpx-sse` for streaming chat responses |
| Error mapping | HTTP status → user-friendly CLI error messages |
| Config | `MO_AGENT_API_URL` env var, default `http://localhost:8000` |

**Dependencies**: Add `httpx>=0.27` and `httpx-sse>=0.4` to `pyproject.toml` under a `[project.optional-dependencies] cli` extra. The CLI package should be installable without pulling in the full `core/` dependency tree: `pip install mo-agent-engine[cli]`.

**Validation**: Unit test with mocked HTTP responses for each method.

---

### B2: Admin API endpoints

**New files**: `api/routers/admin.py`

Endpoints that don't exist yet, required before mo-admin migration:

| Endpoint | What it does | Auth |
|---|---|---|
| `POST /admin/init` | Run DDL migrations | admin role |
| `POST /admin/tokens` | Create API/LLM tokens | admin role |
| `GET /admin/tokens` | List tokens | admin role |
| `GET /admin/audit` | Query audit logs | admin role |
| `POST /admin/prompts/optimize` | Trigger prompt optimization | admin role |
| `GET /admin/feedback/stats` | Feedback statistics | admin role |
| `POST /admin/feedback/export` | Export training data | admin role |

**Validation**: API tests for each endpoint with admin/non-admin JWT.

---

### B3: Migrate mo-agent

**Modified file**: `cli/mo_agent.py`

| Command | Before | After |
|---|---|---|
| `chat` | `ChatLoop` + `get_db_session()` | `api_client.chat_stream()` → render SSE |
| `session list/show` | `SessionManager` + DB | `api_client.list_sessions()` / `.get_session()` |
| `replay` | `stream_replay` + DB | `api_client.replay_session()` |
| `skill list/register/...` | `SkillRegistry` + DB | `api_client.list_skills()` / etc. |
| `model list/show` | DB query | `api_client.list_models()` / etc. |

**Validation**: All existing CLI behaviors work through API. Manual smoke test of each command.

---

### B4: Migrate mo-admin

**Modified file**: `cli/mo_admin.py`

| Command | Before | After |
|---|---|---|
| `init` | DDL via `get_db_session()` | `api_client.admin_init()` |
| `token create/list` | Direct DB insert/query | `api_client.admin_create_token()` / etc. |
| `audit logs` | Direct DB query | `api_client.admin_audit_logs()` |
| `prompt optimize` | `PromptOptimizer` + DB | `api_client.admin_optimize_prompt()` |
| `feedback export/retrain` | Direct DB | `api_client.admin_feedback_*()` |

---

### B5: Remove direct DB path

**Modified files**: `cli/mo_agent.py`, `cli/mo_admin.py`

| Item | Detail |
|---|---|
| Delete | All `from api.database import get_db_session` from CLI |
| Delete | All `from core.*` imports from CLI |
| `--local` flag | Optional dev shortcut, re-enables direct DB path. Not default. |
| Verify | CLI package has zero dependency on `core/` or `api/database.py` |

---

## Execution Order & Dependencies

```
Week 1-2:  A1 (EventPipeline core)     ←── no dependency
           B1 (API client module)       ←── no dependency
           B2 (Admin API endpoints)     ←── no dependency

Week 3:    A2 (Wire ChatLoop/RunEngine) ←── depends on A1
           B3 (Migrate mo-agent)        ←── depends on B1

Week 4:    A3 (Embedding decoupling)    ←── depends on A2
           B4 (Migrate mo-admin)        ←── depends on B1 + B2

Week 5:    A4 (Async snapshot/firewall) ←── depends on A2
           A5 (Replay migration)        ←── depends on A2
           B5 (Remove direct DB path)   ←── depends on B3 + B4

Week 6:    Integration testing + acceptance criteria validation
```

A1-A5 和 B1-B5 两条线完全独立，可以由不同的人并行开发。唯一的交汇点是最终集成测试。

---

## Acceptance Criteria (全局)

### Write Path (Workstream A)

| Metric | Target |
|---|---|
| Hot-path write latency (p95) | < 50ms |
| Hot-path write latency (p99) | < 100ms |
| Background flush latency (p95) | < 300ms |
| Embedding availability lag (p95) | < 500ms |
| Embedding availability lag (p99) | < 2s |
| Event loss rate (durable) | < 0.01% |
| Event loss rate (ephemeral) | < 1% |
| Graceful shutdown flush success | > 99% |
| Replay completeness | 100% full-text, >99% chunk-level |
| Hybrid retrieval recall (no embedding) | No regression vs baseline |

### CLI Architecture (Workstream B)

| Metric | Target |
|---|---|
| CLI → API latency overhead | < 20ms vs direct DB (localhost) |
| JWT auto-refresh | Transparent to user, zero manual re-login |
| Admin RBAC | Non-admin gets 403 on all `/admin/*` endpoints |
| Audit coverage | 100% of CLI operations logged in audit trail |
| Zero DB dependency | CLI package imports zero `core/` or `api/database` modules |
| `--local` dev mode | All commands work in both API and local mode |

---

## Risk Register

| Risk | Impact | Mitigation | Phase |
|---|---|---|---|
| Replay refactor introduces bugs | Stream replay broken | 5 mandatory test scenarios + 100-run regression | A5 |
| Fulltext fallback insufficient | Empty context during embedding lag | Integration test: zero-embedding retrieval must return results | A3 |
| Missed `flush_critical()` point | State machine breaks on crash | "When in doubt, flush" rule; review all state transitions | A2 |
| Admin API surface too large | Phase B2 blocks B4 | Start with 4 critical endpoints, add rest incrementally | B2 |
| CLI SSE streaming complexity | Chat UX regression | Reuse existing `stream_run_events` SSE format | B3 |
| Existing tests break | Regression | Feature flag `EVENT_PIPELINE_ENABLED` for gradual rollout | A2 |

---

## Rollback Strategy

| Phase | Rollback |
|---|---|
| A1 | Additive, no impact on existing code |
| A2 | `EVENT_PIPELINE_ENABLED=false` → synchronous writes |
| A3 | Config flag: read embedding from `conversation_events.embedding` or `event_embeddings` JOIN |
| A4 | Independent, revert to synchronous snapshot/firewall writes |
| A5 | Independent, revert to reading stream events from `conversation_events` |
| B1-B4 | `--local` flag preserves direct DB path throughout migration |
| B5 | Don't delete direct DB imports until all commands verified |

---

## References

- [Write Path Optimization](write-path-optimization.md) — full design, consistency model, failure modes
- [Deployment Architecture §1.1](deployment-architecture.md) — CLI as API client design
- [ARCHITECTURE.md](ARCHITECTURE.md) — system overview, write path optimizations
