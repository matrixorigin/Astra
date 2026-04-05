# Write Path Optimization: Async Event Pipeline

> **Status**: Approved — implementation required  
> **Created**: 2026-02-23  
> **Affects**: EventLogger, RunEngine, ChatLoop, ContextManager, HallucinationFirewall

---

## Problem Statement

A single chat turn produces **15+ synchronous DB round-trips**, each with an independent `commit()`. Every event write generates a **1536-dimension embedding** (29KB serialized string) inline, even for ephemeral stream events that will never be searched.

### Measured Impact (per chat turn, no tool calls)

| Operation | Count | Avg Latency | Payload |
|---|---|---|---|
| `create_stream_event` | 6 | 126ms | 29KB embedding each |
| `_append_event` (run_events) | 4 | 72ms | small JSON |
| `_log_run_event` | 2 | 103ms | 29KB embedding each |
| `save_snapshot` | 1 | 70ms | large JSON |
| `build_context` | 1 | 260ms | read path |
| `create_user_query` | 1 | 70ms | 29KB embedding |
| `create_llm_response` | 1 | 130ms | 29KB embedding |
| `firewall.log_verification` | 1 | 70ms | small |

**Total: ~2.5s per turn, of which ~1.8s is synchronous DB writes.**

### Data Amplification

- 8 events × 29KB embedding = **232KB** of embedding data per turn
- A 10-turn conversation writes **2.3MB** of embeddings
- Only 2 of those 80 events (user_query + llm_response) are ever semantically searched

---

## Industry Analysis

### LangSmith (LangChain) — the reference architecture

LangSmith's tracing pipeline is the industry standard for agent event ingestion. Key design choices (content rephrased for compliance with licensing restrictions):

1. **Fire-and-forget enqueue**: Event creation returns in <1ms. Events are serialized and pushed to an in-memory `PriorityQueue`. The application thread never blocks on I/O.

2. **Background drain thread**: A dedicated thread drains the queue with a 250ms initial wait, then rapid 50ms polls. Batches up to 100 items or a size limit.

3. **Operation merging**: POST (create) + PATCH (update) for the same run are merged into a single POST before submission, reducing API calls by ~20%.

4. **Zstandard compression**: Entire batch is compressed (level 1 for speed), reducing bandwidth 60-80%.

5. **Auto-scaling workers**: When queue depth exceeds 1000, additional worker threads spawn (up to 16). Scale down after 4 consecutive empty drains.

6. **Storage separation**: ClickHouse for high-volume traces (OLAP), PostgreSQL for transactional data. Traces are eventually consistent — visible in UI within 1-2 seconds.

7. **Crash tolerance**: In-memory queue. Process crash = lose buffered events. This is acceptable for observability data.

8. **Embedding is decoupled**: Embeddings are generated asynchronously, not inline with event creation.

Sources: [LangSmith SDK Background Processing](https://deepwiki.com/langchain-ai/langsmith-sdk/2.4-background-processing-and-batching), [LangSmith Architecture](https://docs.smith.langchain.com/self_hosting)

### Other systems

| System | Approach |
|---|---|
| **OpenTelemetry** | BatchSpanProcessor: in-memory queue → background export every 5s or 2048 spans |
| **Datadog APT** | Agent-side buffering → flush every 10s → intake API |
| **Braintrust** | Async logger with background flush, spans buffered in memory |
| **Anthropic (internal)** | Event sourcing with async write-behind to durable store |

**Universal pattern**: Agent event data is treated as a **log stream**, not transactional data. Fire-and-forget enqueue, background batch flush, eventual consistency, crash loss acceptable.

---

## Design: Async Event Pipeline

### Core Principle

**The hot path (chat turn) must never wait for DB I/O on event writes.**

Events are enqueued in-memory (<1μs). A background thread drains, batches, and flushes to DB. The chat turn only blocks on DB for the minimal set of reads it actually needs (build_context, get_tools_schema).

In the [Edge-Cloud Execution](edge-cloud-execution.md) architecture, edge-sourced events (tool_results) arrive via `/chat/turn` and enter this same pipeline on the cloud side. They are tagged `source: "edge"` for audit provenance (see [Trust and Safety § Edge Trust Boundary](trust-and-safety.md)) but follow identical classification, batching, and flush rules.

### Architecture

```
ChatLoop / RunEngine (hot path)
  │
  │  event_logger.emit(event)  ← returns immediately, <1μs
  │
  ▼
┌─────────────────────────────────────────────┐
│  IN-MEMORY EVENT BUFFER                     │
│  asyncio.Queue (unbounded)                  │
│  + run_events dict (for SSE streaming)      │
│                                             │
│  Events are immediately visible for:        │
│  - SSE streaming (in-memory _run_events)    │
│  - Same-process reads                       │
└──────────────────┬──────────────────────────┘
                   │
                   │  Background drain (every 200ms or 50 events)
                   ▼
┌─────────────────────────────────────────────┐
│  FLUSH WORKER (background asyncio task)     │
│                                             │
│  1. Drain queue → batch                     │
│  2. Classify events by tier                 │
│  3. Route events:                           │
│     - conversation_events (critical+durable)│
│     - run_events (all with run_id)          │
│  4. Bulk INSERT + single COMMIT             │
│     (no embedding — fully decoupled)        │
│  5. On failure: retry once, then drop       │
│     (log warning, increment metric)         │
└─────────────────────────────────────────────┘
```

### Event Classification

Events are classified into three tiers at emit time:

| Tier | Event Types | Write to `conversation_events` | Write to `run_events` | Consistency |
|---|---|---|---|---|
| **Critical** | `user_query`, `llm_response` | ✅ | If has `run_id` | Synchronous flush before dependent read |
| **Durable** | `run_started`, `run_completed`, `run_failed`, `run_cancelled`, `run_waiting`, `stream_tool_result`, `plan_created`, `knowledge_extracted` | ✅ | If has `run_id` | Eventual (background flush) |
| **Ephemeral** | `stream_text_delta`, `stream_text_done`, `stream_run_started`, `stream_run_finished`, `stream_tool_call_start`, all other `stream_*` | ❌ | ✅ | Eventual, loss acceptable |

**No tier generates embeddings.** Embeddings are a completely separate async process — see [Deferred Embedding Strategy](#deferred-embedding-strategy).

**`run_events` routing is orthogonal to tier**: any event carrying a `run_id` writes to `run_events`, regardless of tier. The tier determines whether it *also* writes to `conversation_events` and whether the flush is synchronous.

### Synchronous Flush Points

Only **two** places in the hot path require synchronous DB visibility:

1. **After `create_user_query`** — because `build_context` reads this event back for hybrid retrieval. Flush: `event_logger.flush_critical()`.

2. **After `_log_run_event(RUN_COMPLETED/FAILED/CANCELLED)`** — because cross-worker polling reads run status from DB. Flush: `event_logger.flush_critical()`.

Everything else — stream events, tool call logs, snapshots, firewall logs, quality scores — flushes in the background.

**Implementation rule: when in doubt, flush.** If a new code path introduces a state-machine transition (e.g., a future `RUN_WAITING` → `RUN_RESUMED` handoff), add a `flush_critical()` call. The cost is one ~30ms DB round-trip — far cheaper than debugging a broken state chain from a missed flush. The two points above are the minimum; implementations SHOULD add flush points at any "write then immediately read from another process" boundary.

### The EventPipeline

Replaces the current `EventLogger` as the write-path interface:

```python
class EventPipeline:
    """Async event ingestion pipeline.
    
    Hot path: emit() enqueues in-memory, returns immediately.
    Background: drain → classify → batch → flush to DB.
    Embedding: completely separate — not in this pipeline.
    """
    
    DURABLE_TYPES = {
        EventType.RUN_STARTED,
        EventType.RUN_COMPLETED,
        EventType.RUN_FAILED,
        EventType.RUN_CANCELLED,
        EventType.RUN_WAITING,
        EventType.RUN_RESUMED,
        EventType.STREAM_TOOL_RESULT,
        EventType.PLAN_CREATED,
        EventType.PLAN_REVISED,
        EventType.KNOWLEDGE_EXTRACTED,
    }
    
    CRITICAL_TYPES = {
        EventType.USER_QUERY,
        EventType.LLM_RESPONSE,
    }
    # Everything else is ephemeral (conversation_events: no, run_events: yes)
    # Note: run_events routing is by run_id presence, not by tier.
    
    # Flush configuration
    FLUSH_INTERVAL_MS = 200      # Max time before background flush
    FLUSH_BATCH_SIZE = 50        # Max events per batch
    
    def __init__(self, db_factory):
        """
        Args:
            db_factory: Callable that returns a new SQLAlchemy Session.
                        Background thread uses its own session (no sharing).
        """
        self._queue = asyncio.Queue()
        self._db_factory = db_factory
        self._flush_task = None
        self._stats = {"emitted": 0, "flushed": 0, "dropped": 0}
    
    def emit(self, event: ConversationEvent) -> str:
        """Fire-and-forget. Returns event_id immediately."""
        self._queue.put_nowait(event)
        self._stats["emitted"] += 1
        return event.event_id
    
    def flush_critical(self):
        """Synchronous flush for critical events only.
        
        Drains CRITICAL_TYPES events from queue and commits immediately.
        Called at the two sync points (after user_query, after run status).
        No embedding — just the event row.
        """
        ...
    
    async def _flush_loop(self):
        """Background: drain → classify → batch INSERT → commit."""
        db = self._db_factory()
        try:
            while True:
                batch = await self._drain(timeout_ms=self.FLUSH_INTERVAL_MS,
                                          max_items=self.FLUSH_BATCH_SIZE)
                if batch:
                    self._flush_batch(db, batch)
        finally:
            db.close()
    
    def _flush_batch(self, db, events: list[ConversationEvent]):
        """Classify, bulk INSERT, single COMMIT. No embedding."""
        ce_rows = []   # conversation_events
        re_rows = []   # run_events
        
        for event in events:
            et = event.event_type
            
            # Route: conversation_events for critical + durable
            if et in self.CRITICAL_TYPES or et in self.DURABLE_TYPES:
                ce_rows.append(self._to_ce_row(event))  # No embedding
            
            # Route: run_events for anything with a run_id
            run_id = (event.metadata or {}).get("run_id")
            if run_id:
                re_rows.append(self._to_re_row(event, run_id))
        
        # Bulk INSERT — single round-trip per table
        try:
            if ce_rows:
                db.execute(bulk_insert_conversation_events, ce_rows)
            if re_rows:
                db.execute(bulk_insert_run_events, re_rows)
            db.commit()
            self._stats["flushed"] += len(events)
        except Exception as e:
            db.rollback()
            logger.warning(f"Event flush failed ({len(events)} events): {e}")
            self._stats["dropped"] += len(events)
    
    def shutdown(self):
        """Drain remaining events on process exit. Best-effort."""
        ...
```

### Integration with RunEngine

`RunEngine._append_event` currently writes to `run_events` synchronously. After this change:

```python
# Before (current): synchronous DB write per event
def _append_event(self, run_id, sse):
    events = _run_events.setdefault(run_id, [])
    events.append(sse)
    self.db.execute(INSERT_RUN_EVENT, {...})
    self.db.commit()  # 72ms per call

# After: in-memory only, DB write delegated to pipeline
def _append_event(self, run_id, sse):
    events = _run_events.setdefault(run_id, [])
    events.append(sse)
    # DB persistence handled by EventPipeline background flush
    # SSE streaming reads from _run_events (in-memory), not DB
```

SSE streaming (`stream_run_events`) already reads from `_run_events` in-memory first, with DB as fallback for cross-worker. The background flush ensures cross-worker visibility within ~200ms.

### Integration with ChatLoop

ChatLoop currently calls `create_stream_event` (synchronous DB write) for every SSE event. After this change:

```python
# Before: every stream event = synchronous DB write with embedding
text_event = self.event_logger.create_stream_event(
    user_id=user_id, session_id=session_id,
    event_type="stream_text_delta",
    content=json.dumps({"chunk": chunk}),
    ...
)  # 126ms

# After: fire-and-forget emit
text_event = self.event_pipeline.emit_stream_event(
    user_id=user_id, session_id=session_id,
    event_type="stream_text_delta",
    content=json.dumps({"chunk": chunk}),
    ...
)  # <1μs
```

The only synchronous flush in ChatLoop:

```python
# After create_user_query — must be visible for build_context
user_event = self.event_pipeline.emit_user_query(...)
self.event_pipeline.flush_critical()  # ~30ms (1 INSERT + commit, no embedding yet)

# build_context runs here — reads user_query from DB
ctx = self.context_manager.build_context(...)

# Embedding for user_query is generated in background flush
# (build_context uses fulltext search as fallback if embedding not yet available)
```

### Deferred Embedding Strategy

Embeddings are **completely decoupled** from the event write path. They are a derived index, not part of the event record.

The project already has an `event_embeddings` table (`api/models.py:EventEmbedding`) designed for exactly this purpose — but the current code ignores it and inlines embeddings into `conversation_events.embedding` instead. This design corrects that.

**Architecture:**

```
Event write path (hot + background flush):
  conversation_events.embedding = NULL (always)
  
Embedding path (fully async, separate worker):
  1. Background task polls for events with no embedding
     (or subscribes to new-event notifications)
  2. Generates embedding via EmbeddingService
  3. INSERT INTO event_embeddings (event_id, embedding, model_name, ...)
  4. Runs on its own DB session, own schedule, own failure domain
```

**Why a separate table (`event_embeddings`) instead of a column on `conversation_events`:**

1. **Write amplification** — 29KB embedding makes every INSERT a large write. Without it, event INSERTs are <1KB.
2. **Lifecycle separation** — Events are immutable facts. Embeddings are derived data that may be regenerated when the embedding model changes (e.g., upgrading from text-embedding-3-small to a future model).
3. **Index efficiency** — HNSW vector index on a narrow table (event_id + embedding) has much better cache hit rate than on a wide table with 20+ columns.
4. **Independent scaling** — Embedding generation can be scaled independently (dedicated worker, GPU, batch API calls to OpenAI).
5. **Zero hot-path impact** — Event writes never touch vector data. No HNSW index maintenance on the write path.

**Hybrid retrieval adaptation:**

```sql
-- Before: embedding on conversation_events (current)
SELECT e.event_id, e.content,
  0.35 * l2_distance(e.embedding, @query_vec) + ...
FROM conversation_events e
WHERE e.session_id = :sid AND e.embedding IS NOT NULL

-- After: JOIN with event_embeddings
SELECT e.event_id, e.content,
  0.35 * l2_distance(emb.embedding, @query_vec) + ...
FROM conversation_events e
JOIN event_embeddings emb ON e.event_id = emb.event_id
WHERE e.session_id = :sid
```

The JOIN is cheap — `event_embeddings` is indexed by `event_id` (primary key). And the HNSW index on `event_embeddings.embedding` is more efficient because the table is narrow.

**Graceful degradation:** If an event's embedding hasn't been generated yet (async lag), it simply won't appear in the JOIN result. Fulltext search (`MATCH ... AGAINST`) still covers it. This is the same behavior as the current `WHERE embedding IS NOT NULL` filter.

> **⚠️ Implementation constraint: fulltext fallback must be robust.**
> With decoupled embeddings, every code path that uses semantic search MUST have a working fulltext fallback. This is not optional — it is the primary retrieval method during the embedding lag window. Specifically:
> - `hybrid_retrieval.py` must return meaningful results even when `event_embeddings` JOIN yields zero rows (e.g., brand-new session, embedding worker down).
> - `build_context` must never fail or return empty context just because embeddings are unavailable.
> - No business logic should assume "event exists → embedding exists". The correct mental model: **embeddings are a search optimization, not a data dependency.**
> - Integration tests must cover the "zero embeddings available" scenario explicitly.

**Migration:** The `conversation_events.embedding` column can be dropped after migration. Existing embeddings are migrated to `event_embeddings` via a one-time script.

---

## Impact Analysis

### Before (current)

| Metric | Per Turn |
|---|---|
| DB round-trips (commits) in hot path | ~15 |
| Embedding computations in hot path | ~10 |
| Embedding data written | ~290KB |
| Hot path write latency | ~1.8s |

### After

| Metric | Per Turn |
|---|---|
| DB round-trips in hot path | **1** (flush_critical after user_query) |
| Embedding computations in hot path | **0** |
| Embedding data in event INSERTs | **0** (embedding in separate table, async) |
| **Hot path write latency** | **~30ms** |
| Background flush latency (invisible to user) | ~200ms per batch |
| Embedding generation (fully async) | ~200ms, separate worker |

**~60x reduction in hot-path write latency. ~5x reduction in embedding data.**

### Consistency Model

This system has two distinct consistency boundaries:

**Transactionally consistent (strong guarantees):**
- Event fact records in `conversation_events` — once `flush_critical()` returns, the row is committed and visible to all readers (including cross-worker). MVCC snapshot isolation applies. Time-travel (`RESTORE SNAPSHOT`) restores these exactly.
- `knowledge_entries` with inline embeddings — same row, same transaction, same snapshot. Vector search results are transactionally consistent with the knowledge data.

**Eventually consistent (async, may lag):**
- Event embeddings in `event_embeddings` — generated by `EmbeddingWorker` after the event is committed. Typical lag <500ms. During the gap, the event exists but is invisible to vector search. Fulltext search covers it immediately.
- Durable/ephemeral events — flushed in background within ~200ms. Not yet visible to cross-worker DB reads during the window.
- Context snapshots, firewall logs — async, ~200ms lag.

| Data | Consistency | Visible After |
|---|---|---|
| `user_query` event (no embedding) | Synchronous | Immediate |
| `user_query` embedding | Eventual | ~500ms (async worker) |
| `llm_response` event | Eventual | ~200ms (background flush) |
| `llm_response` embedding | Eventual | ~500ms (async worker) |
| Stream events (text_delta, etc.) | Eventual | ~200ms |
| Run status (completed/failed) | Eventual | ~200ms |
| Context snapshots | Eventual | ~200ms |
| Firewall logs | Eventual | ~200ms |
| Knowledge entries + embedding | Synchronous | Immediate |

### Acceptance Criteria

Minimum viable metrics to validate the design goals in production:

| Metric | Target | How to Measure |
|---|---|---|
| Hot-path write latency (p95) | < 50ms | Timer around `flush_critical()` in ChatLoop |
| Hot-path write latency (p99) | < 100ms | Same timer, p99 percentile |
| Background flush latency (p95) | < 300ms | Timer in `_flush_loop()` per batch |
| Embedding availability lag (p95) | < 500ms | `event_embeddings.created_at - conversation_events.created_at` |
| Embedding availability lag (p99) | < 2s | Same delta, p99 |
| Event loss rate (durable tier) | < 0.01% | `emitted - flushed` counter over 24h window |
| Event loss rate (ephemeral tier) | < 1% | Same counter, ephemeral only |
| Graceful shutdown flush success | > 99% | `shutdown_flushed / shutdown_queued` counter |
| Replay completeness | 100% full-text, >99% chunk-level | Compare `llm_response` count vs `run_events` chunk count per run |
| Hybrid retrieval recall (no embedding yet) | No regression vs baseline | A/B: fulltext-only recall during embedding lag window |

**How to validate before production:**
1. Phase 1 unit test: emit 1000 events, verify all flushed within 1s
2. Phase 2 integration test: run 10 chat turns, assert hot-path timer < 50ms each
3. Phase 3 integration test: verify `event_embeddings` rows appear within 500ms of event commit
4. Chaos test: SIGKILL during streaming, verify critical events survived, durable loss < 0.01%

### Failure Modes

| Failure | Impact | Mitigation |
|---|---|---|
| Process crash | Lose buffered events (up to 200ms worth) | See [Crash Recovery Strategy](#crash-recovery-strategy) below |
| DB connection failure | Events accumulate in memory queue | Retry once per batch. If persistent, drop events and log. Queue has no hard size limit but memory pressure triggers backpressure. |
| Embedding service failure | Events written without embedding | Graceful degradation: fulltext search still works. Embedding can be backfilled later. |

#### Crash Recovery Strategy

**Graceful shutdown (SIGTERM, SIGINT):**
`shutdown()` registers via `atexit` and signal handlers. On process exit:
1. Stop accepting new `emit()` calls (raise `PipelineClosed`)
2. Drain remaining queue with a 2-second deadline
3. Flush final batch synchronously (best-effort, single attempt)
4. Log count of flushed vs dropped events

**Hard crash (SIGKILL, OOM):**
In-memory queue is lost. Impact by tier:
- **Critical events**: Already flushed synchronously at the two flush points. No loss.
- **Durable events**: Up to 200ms of events lost. Recoverable: `run_events` in-memory dict (if same process) or client-side retry (if cross-worker). Run status can be reconstructed from the last known state + absence of completion event = "interrupted".
- **Ephemeral events**: Lost. Acceptable — these are stream deltas, reconstructable from `llm_response` full text.

**Upgrade path for zero-loss:**
For deployments that cannot tolerate any durable event loss, the pipeline supports an optional persistent queue backend:
1. **Redis Streams** (recommended): `emit()` writes to Redis Stream instead of in-memory queue. Survives process crashes. Adds ~1ms latency per emit. Flush worker reads from Redis Stream with consumer groups. Already listed in Future Optimizations.
2. **WAL file**: Append events to a local write-ahead log before enqueuing. On restart, replay unflushed entries. Adds ~0.1ms (fsync amortized). Suitable for single-process deployments.

### Backpressure

If the background flush can't keep up (DB down, slow):

1. Queue grows in memory
2. At 10,000 queued events (~10MB): log warning
3. At 100,000 queued events (~100MB): start dropping ephemeral events (stream_text_delta)
4. Critical and durable events are never dropped from queue (but may fail on flush)

---

## Affected Behaviors

### SSE Streaming
**No impact.** `stream_run_events` reads from `_run_events` (in-memory dict). This is unchanged. Cross-worker fallback reads from `run_events` table, which is populated by background flush within ~200ms.

### Replay (`stream_replay.py`)

**Requires update.** Currently reads stream events from `conversation_events`. After this change, ephemeral stream events only exist in `run_events`.

**Replay selection strategy (priority order):**

1. **Full-text replay** (preferred): Read `llm_response` event from `conversation_events`. Contains the complete assistant output. Sufficient for most replay use cases (audit, regression testing, comparison). Always available — critical tier, synchronously flushed.

2. **Chunk-level replay** (when needed): Read `stream_text_delta` / `stream_text_done` events from `run_events`. Required for: timing analysis, streaming behavior regression, token-by-token comparison. Subject to eventual consistency — available after background flush (~200ms).

3. **Fallback**: If `run_events` data is missing (process crash during streaming, or events aged out), fall back to full-text replay from `llm_response`. Log a warning that chunk-level data is unavailable.

**Cross-worker consistency window:**
When replaying a run that was executed on a different worker, `run_events` may have up to 200ms of flush lag. Replay should wait for run completion (check `run_completed` / `run_failed` event in `conversation_events`) before reading `run_events` to ensure all chunks are flushed. If the run is still in progress, replay streams from the live SSE endpoint instead.

> **⚠️ Replay is the highest-risk refactor in this design.**
> `stream_replay.py` changes from a single-table read (`conversation_events`) to a multi-table, multi-strategy read with fallback logic. This is where bugs will hide. Implementation must include:
> - Test: replay a normal completed run → expect chunk-level output matching original stream
> - Test: replay after simulated crash (missing `run_events` chunks) → expect graceful fallback to `llm_response` full text
> - Test: replay of a run from a different worker → expect correct wait-for-completion behavior
> - Test: replay of a run with zero stream events (e.g., tool-only turn) → expect no crash
> - Regression: compare replay output before/after migration for 100 historical runs

### Cross-session Context (`build_context`)
**No impact.** Hybrid retrieval filters `WHERE embedding IS NOT NULL`. Events without embeddings (not yet backfilled, or ephemeral) are excluded. Fulltext search covers the gap.

### Distributed Coordination
**Minimal impact.** `_is_cancelled_in_db`, `restore_run`, `_find_waiting_run_by_handle` query `conversation_events` for run status events. These are classified as Durable and flushed within ~200ms. Cross-worker cancel detection already polls every 0.5s (every 5 events), so 200ms flush delay is invisible.

### Audit Trail
**Strengthened.** `conversation_events` becomes a clean audit log of semantically meaningful events only. No more noise from stream_text_delta. Ephemeral events remain in `run_events` for debugging.

### Memory Governance
**No impact.** Operates on `user_query` and `llm_response` events, which retain embeddings.

### Quality Scoring
**No impact.** `update_quality_score` operates on `llm_response` events.

---

## Implementation Plan

### Phase 1: EventPipeline core (new file)

Create `core/events/pipeline.py` with:
- `EventPipeline` class with `emit()`, `flush_critical()`, `_flush_loop()`
- Event classification (CRITICAL / DURABLE / EPHEMERAL)
- `run_events` routing by `run_id` presence (orthogonal to tier)
- Bulk INSERT for both tables
- Background asyncio task for flush loop
- Graceful shutdown with `atexit`

Files: `core/events/pipeline.py` (new)

### Phase 2: Wire into ChatLoop + RunEngine

1. Replace `EventLogger` usage in `ChatLoop` with `EventPipeline`
2. Replace synchronous `_append_event` DB writes in `RunEngine` with pipeline emit
3. Add `flush_critical()` at the two sync points
4. Keep `EventLogger` for backward compatibility (tests, CLI) — it delegates to pipeline

Files: `core/agent/chat_loop.py`, `core/agent/run_engine.py`, `api/routers/chat.py`

### Phase 3: Embedding decoupling

1. Remove embedding generation from `EventLogger.log_event()` entirely
2. Create `EmbeddingWorker` — async task that polls `conversation_events` for events without embeddings in `event_embeddings`, generates them, and INSERTs into `event_embeddings`
3. Update `hybrid_retrieval.py` to JOIN `event_embeddings` instead of reading `conversation_events.embedding`
4. Migration script: copy existing `conversation_events.embedding` → `event_embeddings`
5. Drop `conversation_events.embedding` column (after migration verified)

Files: `core/events/event_logger.py`, `core/events/embedding_worker.py` (new), `core/context/hybrid_retrieval.py`, `api/models.py`

### Phase 4: Async snapshot + firewall

1. `save_snapshot` and `log_verification` become async (emit to pipeline)

Files: `core/context/manager.py`, `core/verification/firewall.py`

### Phase 5: Replay migration

1. Update `stream_replay.py`: primary path reads `llm_response` from `conversation_events` (full-text replay)
2. Add chunk-level replay path: read `stream_text_delta` from `run_events`, gated on run completion
3. Fallback logic: if `run_events` chunks missing, degrade to full-text with warning

Files: `core/agent/stream_replay.py`

---

## Rollback Strategy

- Phase 1: `EventPipeline` is additive. Old `EventLogger` still works.
- Phase 2: Feature flag `EVENT_PIPELINE_ENABLED=false` falls back to synchronous writes.
- Phase 3-4: Independent of each other.

---

## Future Optimizations (not in scope)

1. **Zstandard compression**: Compress bulk INSERT payloads for large batches (LangSmith does this).
2. **Batch embedding API**: Call OpenAI embeddings API with batch of texts instead of one-by-one. Reduces API calls and cost.
3. **ClickHouse for events**: For very high throughput (>1000 events/sec), move `run_events` to ClickHouse (like LangSmith does). MatrixOne's AP engine may already handle this.
4. **Persistent queue backends**: Redis Streams or local WAL for zero-loss guarantees. See [Crash Recovery Strategy](#crash-recovery-strategy) for details.

---

## References

- [LangSmith SDK Background Processing and Batching](https://deepwiki.com/langchain-ai/langsmith-sdk/2.4-background-processing-and-batching) — fire-and-forget queue, background thread, batch drain, operation merging, zstd compression, auto-scaling workers
- [LangSmith Self-Hosted Architecture](https://docs.smith.langchain.com/self_hosting) — ClickHouse for traces, PostgreSQL for transactions, Redis for queuing
- [LangSmith Trace with API](https://docs.langchain.com/langsmith/trace-with-api) — "SDKs designed with batching and backgrounding to ensure application performance is not impacted"
