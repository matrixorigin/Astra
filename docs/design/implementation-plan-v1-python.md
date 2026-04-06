# Unified Implementation Plan

> **Last Updated**: 2026-02-23
> **Scope**: Two workstreams — Write Path Optimization + CLI SaaS Architecture
> **Design Docs**: [write-path-optimization-v1-python.md](write-path-optimization-v1-python.md), [deployment-architecture.md](deployment-architecture.md) §1.1

---

## Overview

Two independent workstreams that can be developed in parallel. No dependency between them until the final integration phase.

```
Workstream A: Write Path (性能)          Workstream B: CLI Edge-Cloud (SaaS)
─────────────────────────────           ──────────────────────────────────────
A1: EventPipeline core                  B1: Edge tools + API client
A2: Wire into ChatLoop/RunEngine        B2: /chat/turn API + server refactor
A3: Embedding decoupling                B3: EdgeChatLoop (edge agentic loop)
A4: Async snapshot + firewall           B4: Admin API + astra-admin migration
A5: Replay migration                    B5: Remove direct DB path + packaging
         │                                        │
         └──────────── Integration ───────────────┘
              EventPipeline + Edge-Cloud CLI
```

---

## Workstream A: Write Path Optimization

**Goal**: 60x hot-path latency reduction (1.8s → ~30ms per turn).
**Design doc**: [write-path-optimization-v1-python.md](write-path-optimization-v1-python.md)

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

## Workstream B: CLI Edge-Cloud Architecture

**Goal**: Edge-cloud split execution. Edge runs tools locally, cloud handles LLM + platform services. No DB credentials on client.
**Design doc**: [deployment-architecture.md](deployment-architecture.md) §1.1

### The Key Insight (2026-02-25 revision)

The previous B1-B5 plan assumed CLI is a "thin HTTP client" with all execution server-side. This is wrong — the server has no user filesystem. The agentic loop (LLM → tool → LLM → ...) must be driven from the edge because tools execute locally.

New protocol: `POST /chat/turn` — edge sends messages + tool_results per turn, cloud does context enrichment + LLM call + verification, returns text + tool_calls. Edge executes tool_calls locally, loops.

### B1: Edge tool execution + API client

**New files**: `cli/tools/` (local tool implementations), `cli/api_client.py` (updated)

| Item | Detail |
|---|---|
| `cli/tools/file_ops.py` | `read_file`, `write_file`, `str_replace`, `list_dir` — execute on user's filesystem |
| `cli/tools/search.py` | `grep`, `glob` — search user's project files |
| `cli/tools/shell.py` | `bash` — execute commands on user's machine, with safety checks |
| `cli/tools/git.py` | `git_status`, `git_diff`, `git_log`, `git_commit` — local git operations |
| `cli/tools/router.py` | Unified dispatch: tool_name → module.execute(). Returns OpenAI function calling schema for all tools. **Must execute independent tools concurrently** (asyncio.gather / ThreadPool) to minimize ping-pong latency — see [edge-cloud-execution.md §8](edge-cloud-execution.md) |
| `cli/permissions.py` | allow/ask/deny permission rules for tool execution. Interactive confirmation UI |
| `APIClient.chat_turn()` | New method: `POST /chat/turn` with messages + tool_results, returns SSE stream |
| `APIClient` (existing) | Keep existing methods for sessions, skills, models, auth |

**Validation**:
- Unit test: each tool executes correctly on real filesystem (tmp_path)
- Unit test: permission manager blocks dangerous commands, allows safe ones
- Unit test: tool router dispatches to correct module
- Unit test: API client with mocked HTTP for chat_turn

---

### B2: `/chat/turn` API endpoint + server-side ChatLoop refactor

**New file**: `api/routers/chat_turn.py`
**Modified files**: `core/agent/chat_loop.py`

The server-side ChatLoop must be refactored to support per-turn execution (edge sends tool results, server does one LLM round).

| Item | Detail |
|---|---|
| `POST /chat/turn` | Accept `{session_id, messages, tool_results, project_rules}`, return SSE stream with `{text, tool_calls, usage}` |
| Server per-turn | Context assembly → model routing → budget check → LLM call → verification → audit → return |
| Tool schema | Server returns available tool schemas (from skill registry + edge-registered tools) so LLM knows what tools exist |
| Event persistence | Server persists: user_query event, llm_response event, tool_call metadata, decision + snapshot |
| Edge tool results | Server receives tool results from edge, persists as events, includes in next LLM context |
| Project rules | Edge sends project rules (from local `.astra/` / `.astra/` rules files, `CLAUDE.md`, etc.) in first turn; server injects into system prompt |

**Key difference from current `/chat/stream`**: Current endpoint runs the full agentic loop server-side (ChatLoop.run_step_stream). New endpoint runs ONE LLM turn, returns tool_calls to edge, waits for edge to call back with results.

**Validation**:
- Integration test: multi-turn tool use via /chat/turn (mock edge sending tool results)
- Integration test: context enrichment (memory injected into LLM context)
- Integration test: audit trail complete (every turn has decision + snapshot)

---

### B3: EdgeChatLoop — the edge-side agentic loop

**New file**: `cli/edge_loop.py`

| Item | Detail |
|---|---|
| `EdgeChatLoop` | Drives the agentic loop: user input → /chat/turn → tool execution → /chat/turn → ... → final answer |
| Tool execution | Dispatches tool_calls to `cli/tools/router.py`, collects results |
| Permission checking | Before executing each tool, checks `cli/permissions.py`. Interactive Y/N/Always/Deny prompt |
| Streaming render | Renders LLM text as it streams from /chat/turn SSE |
| Message history | Maintains current session messages locally (for display and context) |
| Error handling | Network errors → retry with backoff. Tool errors → return error to LLM. LLM errors → display to user |
| Max turns | Configurable limit (default 50) to prevent infinite loops |

**Validation**:
- Unit test: EdgeChatLoop with mocked API client — verifies loop terminates on final answer
- Unit test: EdgeChatLoop with mocked API client — verifies tool_calls dispatched to router
- Unit test: permission denial stops tool execution, returns denial to LLM
- Integration test: full round-trip with real API server (requires dev environment)

---

### B4: Admin API endpoints + astra-admin migration

**Modified file**: `api/routers/admin.py` (already exists); admin CLI is `rust/crates/astra-admin`

| Endpoint | What it does | Auth |
|---|---|---|
| `POST /admin/init` | Run DDL migrations | admin role |
| `POST /admin/tokens` | Create API/LLM tokens | admin role |
| `GET /admin/tokens` | List tokens | admin role |
| `GET /admin/audit` | Query audit logs | admin role |
| `POST /admin/prompts/optimize` | Trigger prompt optimization | admin role |

Migrate astra-admin commands to use API client instead of direct DB.

**Validation**: API tests for each endpoint with admin/non-admin JWT.

---

### B5: Remove direct DB path + CLI packaging

**Modified files** (historical Python layout; current code: `rust/crates/astra-cli`, `rust/crates/astra-admin`)

| Item | Detail |
|---|---|
| Delete | All `from api.database import get_db_session` from CLI |
| Delete | All `from core.*` imports from CLI (except `cli/tools/` which has no core deps) |
| `--local` flag | Optional dev shortcut, re-enables direct DB path. Not default. |
| Verify | CLI package has zero dependency on `core/` or `api/database.py` |
| Packaging | CLI installable as `pip install astra-cli[cli]` without full server deps |

---

## Execution Order & Dependencies

```
Week 1-2:  A1 (EventPipeline core)     ←── no dependency
           B1 (Edge tools + API client) ←── no dependency

Week 3:    A2 (Wire ChatLoop/RunEngine) ←── depends on A1
           B2 (/chat/turn API)          ←── no dependency (new endpoint)

Week 4:    A3 (Embedding decoupling)    ←── depends on A2
           B3 (EdgeChatLoop)            ←── depends on B1 + B2

Week 5:    A4 (Async snapshot/firewall) ←── depends on A2
           A5 (Replay migration)        ←── depends on A2
           B4 (Admin API + astra-admin)    ←── depends on B1

Week 6:    B5 (Remove direct DB path)   ←── depends on B3 + B4
           Integration testing + acceptance criteria validation
```

A1-A5 和 B1-B5 两条线大部分独立。B2 (/chat/turn) 是新增 API endpoint，不修改现有 /chat/stream。B3 (EdgeChatLoop) 依赖 B1 (edge tools) + B2 (/chat/turn API)。

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
| Edge tool execution | All file/shell/git/search tools execute locally, < 1s each |
| `/chat/turn` round-trip | < 200ms overhead vs direct LLM call (excluding LLM latency) |
| Context enrichment | Memory + few-shot injected per turn (verified in audit snapshot) |
| JWT auto-refresh | Transparent to user, zero manual re-login |
| Admin RBAC | Non-admin gets 403 on all `/admin/*` endpoints |
| Audit coverage | 100% of LLM calls + tool executions logged in audit trail |
| Zero DB dependency | CLI package imports zero `core/` or `api/database` modules |
| Permission system | Dangerous commands blocked; user confirmation for write ops |
| `--local` dev mode | All commands work in both API and local mode |

---

## Risk Register

| Risk | Impact | Mitigation | Phase |
|---|---|---|---|
| Replay refactor introduces bugs | Stream replay broken | 5 mandatory test scenarios + 100-run regression | A5 |
| Fulltext fallback insufficient | Empty context during embedding lag | Integration test: zero-embedding retrieval must return results | A3 |
| Missed `flush_critical()` point | State machine breaks on crash | "When in doubt, flush" rule; review all state transitions | A2 |
| Edge-cloud protocol complexity | Multi-turn state management bugs | Extensive integration tests with mock edge + real server | B2/B3 |
| Tool execution security | Dangerous commands on user machine | Permission system with deny-list + interactive confirmation | B1 |
| Network interruption mid-turn | Lost tool results, broken loop | Edge retries with exponential backoff; idempotent /chat/turn | B3 |
| Context enrichment latency | Slow /chat/turn response | Cache memory search results; incremental context assembly | B2 |
| Admin API surface too large | Phase B4 blocks progress | Start with 4 critical endpoints, add rest incrementally | B4 |
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
| B1 | Additive (new files), no impact on existing CLI |
| B2 | Additive (new endpoint `/chat/turn`), existing `/chat/stream` untouched |
| B3 | EdgeChatLoop is new code; existing `astra chat` via `/chat/stream` still works |
| B4 | `--local` flag preserves direct DB path throughout migration |
| B5 | Don't delete direct DB imports until all commands verified |

---

## References

- [Write Path Optimization](write-path-optimization-v1-python.md) — full design, consistency model, failure modes
- [Deployment Architecture §1.1](deployment-architecture.md) — CLI as API client design
- [ARCHITECTURE.md](ARCHITECTURE.md) — system overview, write path optimizations
