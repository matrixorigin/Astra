# Session Lifecycle Design: From Request-Scoped to Factory-Based

## Problem

The current architecture passes a single `db: Session` into a deep object graph
at request time. Every component stores `self.db = db` and uses it for the
duration of its lifetime. This means:

| Path | Session held for | Description |
|------|-----------------|-------------|
| `start_run` → `bg_db` | up to 30 min (run timeout) | ~95% of time does not need an exclusive connection (waiting for LLM); events are INSERT-ed during streaming but could use short-lived sessions |
| `_build_chat_loop` → request `db` | entire HTTP request | ~90% idle |
| `_run_gate` → gate session | minutes (sandbox + replay) | ~60% idle |
| `GovernanceTaskRunner.run()` | minutes to tens of minutes | ~70% idle |

With N concurrent runs, the system needs N persistent connections. The pool
(currently pool_size=10, max_overflow=20, **30 max** — set by MatrixOne client)
becomes the concurrency ceiling. **This does not scale.**

## Current Architecture

```
HTTP request
  └─ db = SessionLocal()                    ← one session per request
       ├─ EventLogger(db)
       ├─ LLMClient(db)
       ├─ SkillRegistry(db)
       ├─ CodeExecutor(db)
       ├─ ContextManager(db)
       ├─ ToolRegistry()
       ├─ AgentExecutor(db)
       ├─ HallucinationFirewall(db)
       └─ ChatLoop(selector, executor, ...)
            └─ start_run()
                 └─ bg_db = self._new_db()  ← second session, held 30min
                      ├─ _task_db contextvar overrides self.db property
                      ├─ All ChatLoop/component DB calls resolve to bg_db
                      ├─ _append_event → INSERT (buffered)
                      ├─ _flush_run_events → batch commit every 20 events
                      ├─ _is_cancelled_in_db → SELECT every 5 events (shares bg_db)
                      └─ finally: bg_db.close()
                         (no explicit rollback — relies on pool_reset_on_return="rollback")
```

~85 classes across 89 files store `self.db` as an instance attribute. They all
assume the session is valid for their entire lifetime.

## Target Architecture

Replace `db: Session` with `db_factory: Callable[[], Session]`. Each method
that needs DB acquires a short-lived session, commits/rollbacks, and returns
it to the pool immediately.

```
HTTP request
  └─ db_factory = SessionLocal              ← factory, not instance
       ├─ EventLogger(db_factory)
       ├─ LLMClient(db_factory)
       ├─ ...all components receive factory...
       └─ ChatLoop(...)
            └─ start_run()
                 ├─ _append_event():
                 │    buffer in memory (no session)
                 ├─ _flush_run_events(batch):
                 │    db = db_factory()
                 │    try: bulk INSERT batch; db.commit()
                 │    finally: db.close()
                 ├─ _is_cancelled_in_db():
                 │    db = db_factory()
                 │    try: SELECT; return result
                 │    finally: db.close()
                 └─ (no bg_db held for 30min)
```

## Commit Semantics: Caller-Commit

The `_db()` context manager uses **caller-commit** semantics:

```python
@contextmanager
def _db(self) -> Iterator[Session]:
    db = self._db_factory()
    try:
        yield db
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()
```

- Normal exit **without** `db.commit()` → `db.close()` returns connection
  to pool; uncommitted changes are discarded (SQLAlchemy's
  `pool_reset_on_return="rollback"` resets the transaction).
- Caller must explicitly call `db.commit()` when writes should persist.
- On exception: explicit `rollback()` before `close()` to ensure clean state.

**Why caller-commit over auto-commit**: Transaction boundaries must be explicit.
Auto-commit would silently persist partial writes in multi-statement operations.
The rule is simple: **if you write, you commit.**

Code review checklist item: every `with self._db() as db:` block that does
INSERT/UPDATE/DELETE must have an explicit `db.commit()`.

## Batch Commit Strategy

Current `RunEngine` buffers events in memory and batch-commits every 20 events
via `_flush_run_events`. This optimization must be preserved.

After migration, `_flush_run_events` acquires a session, bulk-inserts the
buffered batch, commits, and releases:

```python
def _flush_run_events(self):
    batch = self._pending_events[:_RUN_EVENT_FLUSH_SIZE]
    if not batch:
        return
    with self._db() as db:
        db.bulk_save_objects(batch)
        db.commit()
    self._pending_events = self._pending_events[_RUN_EVENT_FLUSH_SIZE:]
```

`_append_event` remains a pure in-memory buffer — no session needed.
This preserves the current batch commit behavior with zero regression.

## Components That Need Long Sessions (Exceptions)

These **cannot** use session-per-operation:

| Component | Reason | Duration |
|-----------|--------|----------|
| `GovernanceTaskRunner.run()` | DistributedLock row acts as advisory lock; releasing session drops the lock | minutes–tens of minutes |
| `GateTrigger._run_gate()` | Same distributed lock + passes session to RegressionGate → Sandbox chain | minutes |
| `Sandbox.create()` / `delete()` | Multi-step DDL (DROP DB → CREATE DB → CREATE PITR → branch tables) requires same connection for consistent cleanup on failure | seconds–minutes |

Note: `EventPipeline._flush_loop` has been migrated to session-per-flush
(acquires session per batch, releases after commit). It is no longer a
long-session holder.

## Must-Audit Methods (Cannot Split Session)

These methods have read-then-write or multi-statement transaction semantics
that require a single session scope. They must use a single `with self._db()`
block encompassing all steps:

| Method | Pattern | Why single session |
|--------|---------|-------------------|
| `RunEngine._try_claim_resume()` | SELECT MIN(idx) → INSERT | Optimistic lock: read and write must see consistent state |
| `SkillManager.install()` | check permission → check existing → add + commit | IntegrityError handling depends on same session's rollback state |
| `GovernanceTaskRunner._try_acquire()` | INSERT → on conflict → SELECT + UPDATE | CAS semantics require same connection |
| `Sandbox.create()` | DROP DB → CREATE DB → CREATE PITR → INSERT metadata | Partial failure cleanup needs same connection |
| `SessionManager.close_session()` | update status + commit → score → extract knowledge → callback | Steps 2-4 depend on step 1's commit, but step 1 already commits explicitly. Safe to split: step 1 gets its own session, steps 2-4 get independent sessions. Listed here as reminder to verify during migration |
| `RunEngine._cancel_workflow()` | UPDATE workflow_runs + commit via `self.db` | `self.db` resolves via contextvar — may point to `bg_db` or `_default_db` depending on call context. Must use independent session to avoid polluting caller's transaction |
| `GovernanceTaskRunner._run_eval_daily()` | Receives outer lock-holding session; Phase 2-4 use it for ConfidenceCalibrator, InputFaceLearner, ToolRegistry | If a phase rollbacks, it affects subsequent phases and the lock. Migration should give Phase 2-4 independent sessions (like Phase 1 already does) |
| `_trigger_loop` (api/main.py) | `get_due_triggers` → loop `claim_and_advance` + `fire_trigger` on same session | If one trigger's `fire_trigger` fails and rollbacks, it corrupts claim state of subsequent triggers. Migration should use session-per-trigger |

## Migration Strategy

### Phase 1: `DbConsumer` base class

```python
# core/db_consumer.py
from contextlib import contextmanager
from typing import Callable, Iterator
from sqlalchemy.orm import Session

class DbConsumer:
    """Base for components that need DB access."""

    def __init__(self, db_factory: Callable[[], Session]):
        self._db_factory = db_factory

    @contextmanager
    def _db(self) -> Iterator[Session]:
        db = self._db_factory()
        try:
            yield db
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()
```

### Phase 2: Migrate high-impact components first

Priority order (by connection hold time × frequency):

1. **`RunEngine`** — eliminate `bg_db`; `_flush_run_events` and
   `_is_cancelled_in_db` use `_db()`. `_append_event` stays in-memory.
2. **`EventLogger`** — used everywhere, short operations, easy win.
3. **`ChatLoop` internals** — `run_step` / `run_step_stream` DB access.
4. **`_build_chat_loop` components** — ToolRegistry, ContextManager,
   AgentExecutor, HallucinationFirewall, etc.
5. **`_trigger_loop`** — change to session-per-trigger.
6. **`_run_eval_daily`** — give Phase 2-4 independent sessions.
7. **Remaining ~80 components** — most are request-scoped, low urgency.

### Phase 3: Remove raw `db: Session` parameter

Once all callers pass `db_factory`, remove the `db` parameter from
constructors.

### Phase 4: Endpoint-level changes

```python
# Before
@app.post("/chat")
def chat(db: Session = Depends(get_db_session)):
    engine = RunEngine(db, chat_loop_factory=_build_chat_loop)

# After
@app.post("/chat")
def chat():
    engine = RunEngine(SessionLocal, chat_loop_factory=_build_chat_loop)
```

## Test Migration Strategy

Current test isolation relies on a single session with rollback:

```python
@pytest.fixture
def db_session(test_session_factory):
    session = test_session_factory()
    yield session
    session.rollback()
    session.close()
```

After migration, components call `db_factory()` which would create new sessions
that can't see uncommitted test data.

**Solution: factory returns the test session, with close() no-op'd and
leak detection.**

```python
@pytest.fixture
def db_factory(db_session):
    """Factory that always returns the same test session.

    close() is no-op'd to prevent DbConsumer._db() from closing the shared
    test session. A counter tracks factory vs close calls to detect leaks.
    """
    original_close = db_session.close
    call_count = 0
    close_count = 0

    def factory():
        nonlocal call_count
        call_count += 1
        db_session.close = _counted_close
        return db_session

    def _counted_close():
        nonlocal close_count
        close_count += 1

    yield factory

    db_session.close = original_close
    assert call_count == close_count, (
        f"Session leak: factory called {call_count}x but close called {close_count}x"
    )
```

Note on `SessionLocal` direct callers: code that calls `SessionLocal()` directly
(e.g., `EventPipeline(SessionLocal)`, `GateTrigger(db_factory=SessionLocal)`)
is covered by `conftest.py`'s `patch_db_engine` fixture which replaces
`database.SessionLocal`. After migration, these become explicit `db_factory`
parameters, making test injection straightforward.

## EventPipeline Lifecycle

`_build_chat_loop` creates an `EventPipeline` and calls `start()` per request.
Shutdown happens in `RunEngine.start_run` finally block. If the request fails
before entering `start_run` (e.g., during `_ensure_session`), the pipeline
and its background task leak.

This is a pre-existing bug. The factory-based model enables a fix: pipeline
should be created lazily on first `emit()` call, or be a singleton shared
across requests (with thread-safe queue, which it already has).

Decision: **singleton EventPipeline** is preferred. The pipeline's asyncio
queue and background task are already thread-safe. A single pipeline avoids
per-request task creation overhead and eliminates the leak-on-early-failure
problem.

Singleton shutdown semantics: the pipeline's lifecycle is bound to the
**process**, not to individual runs. `RunEngine.start_run` finally block
must NOT call `pipeline.shutdown()` on a singleton — only the process
shutdown handler (`_atexit_flush` / lifespan `shutdown`) should do that.

## Async Extension Point

The factory signature `Callable[[], Session]` is sync-only. The codebase
uses asyncio extensively (`start_run` is async, `run_step_stream` is async
generator), but all DB operations are currently synchronous.

Moving to `AsyncSession` is a separate, larger migration. To avoid a second
full-codebase migration later, `DbConsumer` should pre-define the async
interface even if unimplemented now:

```python
class DbConsumer:
    @contextmanager
    def _db(self) -> Iterator[Session]:
        ...  # sync, as above

    @asynccontextmanager
    async def _async_db(self) -> AsyncIterator[AsyncSession]:
        """Reserved for future async session support. Not implemented yet."""
        raise NotImplementedError(
            "Async sessions not yet supported. Use _db() instead."
        )
```

This way, when async migration happens, callers switch from `with self._db()`
to `async with self._async_db()` without changing the base class contract.

## Connection Pool Sizing After Migration

Estimation assumptions:
- LLM streaming: ~50 tokens/s → ~50 events/s per run
- Batch size: 20 events per flush → ~2.5 flushes/s per run
- INSERT + commit latency: ~2ms normal, ~10ms under load
- `_is_cancelled_in_db`: 1 SELECT per 5 events → 10/s per run, ~1ms each
- Each operation holds a connection for ~1-10ms

| Scenario | Before (connections) | After (normal 2ms) | After (worst-case 10ms) |
|----------|---------------------|---------------------|------------------------|
| 100 concurrent runs (flush) | 100 | ~5 | ~25 |
| 100 concurrent runs (cancel check) | 0 additional (shares bg_db) | ~1 | ~5 |
| 10 governance tasks | 10 | 10 (lock-bound) | 10 |
| Health check every 10s | 0 (fixed) | 0 | 0 |
| **Total** | **110+** | **~16** | **~40** |

Target pool config: `pool_size=20, max_overflow=20` should handle hundreds
of concurrent runs. Under sustained high latency, increase `max_overflow`.

## Risks

1. **Transaction semantics change**: Components that do read-then-write across
   multiple methods will see different snapshots if split across sessions.
   See "Must-Audit Methods" section for the complete list.

2. **Test fixtures**: Solved by factory-returns-test-session pattern with
   leak detection counter (see "Test Migration Strategy").

3. **Migration duration**: ~85 classes to migrate. Can be done incrementally —
   `DbConsumer` coexists with raw `db: Session` during transition.

4. **Batch commit regression**: Solved by keeping in-memory buffer +
   session-per-flush pattern (see "Batch Commit Strategy").

5. **`_is_cancelled_in_db` decoupling**: Currently shares `bg_db` with event
   writes. After migration, cancel checks use independent sessions. This
   eliminates potential implicit-flush interference between cancel checks
   and event writes — a net improvement.

## Rollback Plan

If migrating a component causes performance regression or correctness issues:

1. `DbConsumer` subclasses accept an optional `db: Session` in constructor.
   Pass a raw session to revert that component to long-lived mode.
2. Environment variable `DB_CONSUMER_LEGACY=component1,component2` can
   force specific components back to legacy mode without code changes.
3. Each Phase 2 component is migrated in its own commit, enabling
   per-component revert via `git revert`.

## Monitoring

Post-migration, monitor these metrics to validate the design:

| Metric | Source | Expected change |
|--------|--------|----------------|
| Pool checkout wait time | SQLAlchemy `pool.events.checkout` | Should decrease (less contention) |
| Pool overflow frequency | `pool._overflow` counter | Should decrease |
| Session avg hold time | Instrument `DbConsumer._db()` enter/exit | Should be <50ms for most operations |
| Active connections | `pool.checkedout()` | Peak should drop from N-concurrent-runs to ~10-20 |
| Event flush latency | `EventPipeline.stats` | Should stay stable (no regression from session-per-flush) |

## Migration Tracking

| Component | Phase | Status | Notes |
|-----------|-------|--------|-------|
| `EventPipeline._flush_loop` | 1 | ✅ Done | Session-per-flush applied |
| `DbConsumer` base class | 1 | ✅ Done | `core/db_consumer.py` — contextmanager with caller-commit |
| `RunEngine` (bg_db) | 2 | ✅ Done | bg_db eliminated, contextvars removed, session-per-operation |
| `RunEngine._cancel_workflow` | 2 | ✅ Done | Uses `with self._db()` independent session |
| `_trigger_loop` | 2 | ✅ Done | Session-per-trigger; `fire_trigger` takes `db_factory` |
| `EventLogger` | 2 | ✅ Done | Extends DbConsumer; `from_session()` for legacy callers |
| `ChatLoop` | 2 | ✅ Done | `_build_chat_loop` already receives db_factory; fixed `streaming.py` raw-Session caller |
| `_run_eval_daily` Phase 2-4 | 2 | ✅ Done | Each phase gets independent session from db_factory |
| `ToolRegistry` | 3 | ✅ Done | Uses `db_factory()` with manual close |
| `ContextManager` | 3 | ✅ Done | Extends DbConsumer; uses `_db()` context manager |
| `AgentExecutor` | 3 | ✅ Done | Extends DbConsumer; uses `_db()` context manager |
| `HallucinationFirewall` | 3 | ✅ Done | Extends DbConsumer; uses `_db()` context manager |
| `LLMClient` | 3 | ✅ Done | Extends DbConsumer; uses `_db()` context manager |
| `SkillRegistry` | 3 | ✅ Done | Uses `db_factory()` with manual close |
| Remaining ~75 classes | 4 | ✅ Done | All DbConsumer subclasses use `_db()` ctx mgr; all endpoints use `SessionLocal` directly |

## Non-Goals

- Connection pooling middleware (PgBouncer-style) — not needed if sessions
  are short-lived
- Read replicas — orthogonal concern
- Async sessions — separate migration (see "Async Extension Point")
