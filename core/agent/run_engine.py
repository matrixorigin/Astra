"""RunEngine — drives AgentRun execution, decoupled from HTTP lifecycle.

Distributed-safe: all coordination through DB, no cross-worker in-memory deps.
"""

import asyncio
import contextvars
import gc
import json
from collections.abc import AsyncIterator
from datetime import datetime, timezone

from sqlalchemy import text
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session

from core.agent.run import AgentRun, RunStatus, RunTrigger
from core.events.event_logger import EventLogger
from core.events.models import EventType, StreamEvent, StreamEventType
from core.logging_config import get_logger

# Per-task DB session override (set by start_run, used by internal methods)
_task_db: contextvars.ContextVar[Session | None] = contextvars.ContextVar("_task_db", default=None)
_task_event_logger: contextvars.ContextVar[EventLogger | None] = contextvars.ContextVar("_task_event_logger", default=None)

logger = get_logger(__name__)

# In-memory: only for THIS worker's active runs (not shared across workers)
_active_runs: dict[str, AgentRun] = {}
_run_events: dict[str, list[dict]] = {}  # local buffer, also persisted to DB
_run_waiters: dict[str, asyncio.Event] = {}
_run_tasks: dict[str, asyncio.Task] = {}
_child_runs: dict[str, set[str]] = {}  # parent_run_id → {child_run_ids}
_fan_in_tasks: set[asyncio.Task] = set()  # Track fan-in tasks for cleanup


def cleanup_fan_in_tasks() -> None:
    """Cancel all pending fan-in tasks. Call during shutdown or test teardown."""
    for t in list(_fan_in_tasks):
        if not t.done():
            t.cancel()
    _fan_in_tasks.clear()


# Max size for resume user_input to prevent token explosion on adversarial loops
_MAX_RESUME_INPUT_CHARS = 4000
# Max completed runs to keep in memory before cleanup
_MAX_COMPLETED_RUNS = 500
# Periodic GC interval in seconds
_GC_INTERVAL_SECONDS = 300
# Batch flush threshold for run_events (streaming events)
_RUN_EVENT_FLUSH_SIZE = 20

# Global GC task reference
_gc_task: asyncio.Task | None = None


async def _periodic_gc() -> None:
    """Clean up completed runs periodically to prevent memory leaks."""
    while True:
        try:
            await asyncio.sleep(_GC_INTERVAL_SECONDS)
            RunEngine._maybe_gc()
            gc.collect()
            logger.debug(f"Periodic GC: {len(_active_runs)} active runs, {len(_run_events)} event buffers")
        except asyncio.CancelledError:
            logger.info("Periodic GC task cancelled")
            break
        except Exception as e:
            logger.error(f"Periodic GC error: {e}", exc_info=True)


def _start_gc_task() -> None:
    """Start periodic GC task if not already running."""
    global _gc_task
    if _gc_task is None or _gc_task.done():
        coro = _periodic_gc()
        try:
            _gc_task = asyncio.create_task(coro)
            logger.info("Started periodic GC task")
        except RuntimeError:
            coro.close()
            logger.warning("Cannot start GC task: no running event loop")


class RunEngine:
    """Drives AgentRun execution. Not bound to HTTP request lifecycle."""

    def __init__(self, db: Session, chat_loop_factory=None):
        """
        Args:
            db: Default DB session.
            chat_loop_factory: Callable(Session) -> ChatLoop.  Injected by
                the API layer to avoid core → API circular dependency.
        """
        self._default_db = db
        self._default_event_logger = EventLogger(db)
        self._chat_loop_factory = chat_loop_factory
        # Per-instance counter for batched event commits.
        # Must be instance-level to avoid race conditions when multiple
        # RunEngine instances operate concurrently with different DB sessions.
        self._pending_event_count = 0
        # Start GC task on first engine instantiation
        _start_gc_task()

    @property
    def db(self) -> Session:
        """Return per-task DB session if set, otherwise the default."""
        return _task_db.get() or self._default_db

    @db.setter
    def db(self, value: Session) -> None:
        self._default_db = value

    @property
    def event_logger(self) -> EventLogger:
        """Return per-task event logger if set, otherwise the default."""
        return _task_event_logger.get() or self._default_event_logger

    @event_logger.setter
    def event_logger(self, value: EventLogger) -> None:
        self._default_event_logger = value

    def _new_db(self) -> Session:
        """Create a fresh DB session for background tasks."""
        from api.database import SessionLocal
        return SessionLocal()

    # ── Public API ────────────────────────────────────────────

    def create_run(
        self,
        session_id: str,
        user_id: str,
        user_input: str,
        agent_id: str = "dev-agent",
        parent_run_id: str | None = None,
        trigger: RunTrigger = RunTrigger.USER_MESSAGE,
        context: dict | None = None,
    ) -> AgentRun:
        """Create a new AgentRun and persist the run_started event."""
        run = AgentRun(
            session_id=session_id,
            user_id=user_id,
            user_input=user_input,
            agent_id=agent_id,
            parent_run_id=parent_run_id,
            trigger=trigger,
            context=context,
        )
        self._log_run_event(run, EventType.RUN_STARTED)
        _active_runs[run.run_id] = run
        _run_events[run.run_id] = []
        _run_waiters[run.run_id] = asyncio.Event()
        return run

    async def create_child_run(
        self,
        parent_run_id: str,
        agent_id: str,
        task: str,
        context: dict | None = None,
    ) -> AgentRun:
        """Create and start a child run. Parent tracks it for fan-in."""
        parent = _active_runs.get(parent_run_id)
        if not parent:
            raise ValueError(f"Parent run {parent_run_id} not found")

        ctx = dict(context or {})
        # Load agent config from DB
        self._apply_agent_config(agent_id, ctx)

        # Propagate causal chain from parent for audit traceability
        parent_ctx = parent.context or {}
        if "_causal_chain_id" in parent_ctx:
            ctx.setdefault("_causal_chain_id", parent_ctx["_causal_chain_id"])

        child = self.create_run(
            session_id=parent.session_id,
            user_id=parent.user_id,
            user_input=task,
            agent_id=agent_id,
            parent_run_id=parent_run_id,
            trigger=RunTrigger.USER_MESSAGE,
            context=ctx,
        )
        _child_runs.setdefault(parent_run_id, set()).add(child.run_id)

        # Start child in background
        task_obj = asyncio.create_task(self.start_run(child))
        _run_tasks[child.run_id] = task_obj
        return child

    def _load_agent_prompt(self, agent_id: str) -> str | None:
        """Load system_prompt from agents table config."""
        config = self._load_agent_config(agent_id)
        return config.get("system_prompt") if config else None

    def _load_agent_config(self, agent_id: str) -> dict | None:
        """Load agent_config from agents table."""
        try:
            row = self.db.execute(
                text("SELECT agent_config FROM agents WHERE agent_id = :aid"),
                {"aid": agent_id},
            ).fetchone()
            if row and row[0]:
                return row[0] if isinstance(row[0], dict) else json.loads(row[0])
        except Exception as e:
            logger.warning(f"Failed to load agent config for {agent_id}: {e}")
        return None

    def _apply_agent_config(self, agent_id: str, ctx: dict) -> None:
        """Load agent config and inject into context (system_prompt, model, etc.)."""
        config = self._load_agent_config(agent_id)
        if not config:
            return
        if config.get("system_prompt") and "system_prompt" not in ctx:
            ctx["system_prompt"] = config["system_prompt"]
        if config.get("allowed_tools"):
            ctx.setdefault("allowed_tools", config["allowed_tools"])
        if config.get("model") and "model" not in ctx:
            ctx["model"] = config["model"]
            ctx["_model_source"] = "agent_config"  # Audit: tracks where model was resolved from
            logger.info(f"Agent {agent_id} using model: {config['model']}")
        if config.get("model_constraints"):
            ctx.setdefault("model_constraints", config["model_constraints"])

    async def start_run(self, run: AgentRun) -> None:
        """Execute an AgentRun using ChatLoop. Streams events to buffer.
        
        Uses a dedicated DB session via contextvars so concurrent runs
        each get their own session without overwriting shared state.
        """
        run.status = RunStatus.RUNNING
        bg_db = self._new_db()
        bg_event_logger = EventLogger(bg_db)
        tok_db = _task_db.set(bg_db)
        tok_el = _task_event_logger.set(bg_event_logger)
        loop = None
        try:
            # Load agent config and inject model if not already set
            if run.agent_id:
                run.context = run.context or {}
                self._apply_agent_config(run.agent_id, run.context)
            
            # Build ChatLoop via injected factory (preferred) or lazy import (backward compat)
            factory = getattr(self, '_chat_loop_factory', None)
            if factory:
                loop = factory(bg_db)
            else:
                from api.routers.chat import _build_chat_loop
                loop = _build_chat_loop(bg_db)
            loop._current_run_id = run.run_id

            coro = self._consume_stream(loop, run)
            timeout = (run.context or {}).get("run_timeout_seconds", 1800)
            await asyncio.wait_for(coro, timeout=timeout)

            if run.status == RunStatus.RUNNING:
                self._complete_run(run)
        except asyncio.TimeoutError:
            logger.error(f"Run {run.run_id} timed out after {timeout}s")
            run.status = RunStatus.FAILED
            self._log_run_event(run, EventType.RUN_FAILED, {"error": f"Run timed out after {timeout}s"})
            self._append_event(run.run_id, {
                "event_type": "run_error", "data": {"error": f"Run timed out after {timeout}s"},
                "run_id": run.run_id,
            })
            raise  # Re-raise to ensure proper cleanup
        except asyncio.CancelledError:
            run.status = RunStatus.CANCELLED
            run._cancelled_externally = True  # Skip fan-in; parent handles it
            self._log_run_event(run, EventType.RUN_CANCELLED)
            raise  # Re-raise to ensure proper cleanup
        except Exception as e:
            logger.error(f"Run {run.run_id} failed: {e}", exc_info=True)
            run.status = RunStatus.FAILED
            self._log_run_event(run, EventType.RUN_FAILED, {"error": str(e)})
            self._append_event(run.run_id, {
                "event_type": "run_error", "data": {"error": str(e)},
                "run_id": run.run_id,
            })
            raise  # Re-raise to ensure proper cleanup
        finally:
            # Flush any remaining buffered run_events before cleanup
            self._flush_run_events()
            # Shutdown EventPipeline to release its DB session and background task
            try:
                _pipeline = getattr(getattr(loop, 'event_logger', None), '_pipeline', None) if loop else None
                if _pipeline:
                    _shutdown_task = _pipeline.shutdown()
                    if _shutdown_task:
                        try:
                            await asyncio.wait_for(_shutdown_task, timeout=2.0)
                        except (asyncio.CancelledError, asyncio.TimeoutError):
                            pass
            except Exception:
                pass
            _run_tasks.pop(run.run_id, None)
            _run_waiters.get(run.run_id, asyncio.Event()).set()
            # Fan-in: if this is a child run that ended, check parent
            # Skip if externally cancelled — parent's cancel_run already handles children
            if run.parent_run_id and run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED) \
                    and not getattr(run, '_cancelled_externally', False):
                try:
                    await self._check_fan_in(run.parent_run_id)
                except Exception:
                    pass  # Best-effort; don't let fan-in failure break cleanup
            # Cleanup completed runs to prevent memory leak
            if run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                self._maybe_gc()
            # Close background DB session and restore contextvars
            try:
                bg_db.close()
            except Exception:
                pass
            _task_db.reset(tok_db)
            _task_event_logger.reset(tok_el)

    async def _consume_stream(self, loop, run: AgentRun) -> None:
        """Consume ChatLoop stream, parking on wait_for signals.

        Checks for DB cancellation between events so cross-worker cancel
        is detected even for parent runs on remote workers.
        """
        event_count = 0
        async for event in loop.run_step_stream(
            user_input=run.user_input,
            session_id=run.session_id,
            user_id=run.user_id,
            context=run.context,
        ):
            # Cross-worker cancel: check DB periodically for ALL runs
            # (not just child runs — parent runs may be cancelled from
            # another API replica that wrote a RUN_CANCELLED event to DB)
            event_count += 1
            if event_count % 5 == 0 and self._is_cancelled_in_db(run.run_id):
                run.status = RunStatus.CANCELLED
                self._log_run_event(run, EventType.RUN_CANCELLED)
                return

            sse = self._stream_event_to_dict(event, run.run_id)
            self._append_event(run.run_id, sse)

            if event.data.get("wait_for"):
                run.status = RunStatus.WAITING
                run.waiting_for = event.data["wait_for"]
                self._log_run_event(run, EventType.RUN_WAITING, {
                    "waiting_for": run.waiting_for,
                })
                return

    async def resume_run(self, run_id: str, result: dict) -> None:
        """Resume a waiting run. Distributed-safe with optimistic locking."""
        run = _active_runs.get(run_id)

        # Distributed: run might be on another worker — restore from DB
        if not run:
            run = self.restore_run(run_id)
            if run and run.status == RunStatus.WAITING:
                _active_runs[run_id] = run
                _run_events.setdefault(run_id, [])
                _run_waiters.setdefault(run_id, asyncio.Event())
            else:
                logger.warning(f"Cannot resume run {run_id}: not found or not waiting")
                return

        if run.status != RunStatus.WAITING:
            logger.warning(f"Cannot resume run {run_id}: status={run.status}")
            return

        # Optimistic lock: only one worker can claim this resume
        if not self._try_claim_resume(run_id):
            logger.info(f"Run {run_id} already claimed by another worker")
            return

        # Check if cancelled while waiting
        if self._is_cancelled_in_db(run_id):
            run.status = RunStatus.CANCELLED
            self._log_run_event(run, EventType.RUN_CANCELLED)
            _run_waiters.get(run_id, asyncio.Event()).set()
            return

        run.status = RunStatus.RUNNING
        waiting_for = run.waiting_for
        run.waiting_for = None
        self._log_run_event(run, EventType.RUN_RESUMED, {"result": result})

        run.context = run.context or {}
        run.context["resumed_from"] = waiting_for
        run.context["async_result"] = result

        # Build resume input — keep original_input stable to prevent token explosion
        result_summary = json.dumps(result, default=str)[:2000]
        original_input = run.context.get("_original_input", run.user_input)
        run.context["_original_input"] = original_input  # preserve for future resumes
        run.user_input = (
            f"[Async result from {waiting_for}]:\n{result_summary}\n\n"
            f"Original task: {original_input}"
        )[:_MAX_RESUME_INPUT_CHARS]

        self._append_event(run.run_id, {
            "event_type": "tool_result",
            "data": {"result": result},
            "run_id": run.run_id,
        })
        await self.start_run(run)

    def cancel_run(self, run_id: str) -> bool:
        run = _active_runs.get(run_id)
        if not run:
            # Run not on this worker — write cancel event to DB so the
            # owning worker picks it up via periodic _is_cancelled_in_db check.
            restored = self.restore_run(run_id)
            if not restored or restored.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                return False
            return self._write_cancel_event_for_run(
                run_id, restored.session_id, restored.user_id,
            )
        if run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
            return False
        run.status = RunStatus.CANCELLED
        self._log_run_event(run, EventType.RUN_CANCELLED)

        # Cancel the asyncio task
        task = _run_tasks.pop(run_id, None)
        if task and not task.done():
            task.cancel()

        # Cancel children (local + write DB for cross-worker)
        children = _child_runs.pop(run_id, set())
        # Also check DB for children on other workers
        if not children:
            children = self._get_child_run_ids_from_db(run_id)
        for cid in children:
            child_task = _run_tasks.pop(cid, None)
            if child_task and not child_task.done():
                child_task.cancel()
            child = _active_runs.get(cid)
            if child and child.status not in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                child.status = RunStatus.CANCELLED
                self._log_run_event(child, EventType.RUN_CANCELLED)
            elif not child:
                # Cross-worker: write cancel event so other worker detects it
                self._write_cancel_event_for_run(cid, run.session_id, run.user_id)

        # Propagate to workflow
        if run.waiting_for and run.waiting_for.startswith("workflow:"):
            wf_id = run.waiting_for.split(":", 1)[1]
            self._cancel_workflow(wf_id)

        _run_waiters.get(run_id, asyncio.Event()).set()
        return True

    def _cancel_workflow(self, wf_id: str) -> None:
        """Propagate cancel to a workflow and its in-memory state."""
        from core.agent.async_tools import _workflow_runs, _workflow_waits
        entry = _workflow_runs.pop(wf_id, None)
        if entry and entry.get("engine"):
            entry["engine"].cancel(entry["workflow"].name)
        to_remove = [h for h, wid in _workflow_waits.items() if wid == wf_id]
        for h in to_remove:
            _workflow_waits.pop(h, None)
        # Also mark in DB so other workers see it
        try:
            self.db.execute(
                text("UPDATE workflow_runs SET status='cancelled', error='Cancelled by user' "
                     "WHERE run_id = :wf_id AND status IN ('running','waiting')"),
                {"wf_id": wf_id},
            )
            self.db.commit()
        except Exception as e:
            self.db.rollback()
            logger.error(f"Failed to cancel workflow {wf_id} in DB: {e}")

    async def on_job_completed(self, job_id: str, result: dict) -> bool:
        return await self.resolve_handle(f"job:{job_id}", {"job_id": job_id, **result})

    async def resolve_handle(self, handle: str, result: dict) -> bool:
        """Resolve any wait handle. Distributed-safe."""
        from core.agent.async_tools import get_async_tool_registry, resume_workflow

        # 1. Workflow inner wait (in-memory first, then DB fallback)
        if await resume_workflow(handle, result):
            return True

        # 2. In-memory handle → run
        run_id = get_async_tool_registry().resolve_handle(handle)

        # 3. DB fallback: find run waiting for this handle
        if not run_id:
            run_id = self._find_waiting_run_by_handle(handle)

        if not run_id:
            logger.warning(f"No run waiting for handle {handle}")
            return False
        await self.resume_run(run_id, result)
        return True

    def get_run(self, run_id: str) -> AgentRun | None:
        return _active_runs.get(run_id)

    def get_run_events(self, run_id: str, after_index: int = 0) -> list[dict]:
        """Get events — local buffer first, DB fallback for cross-worker."""
        events = _run_events.get(run_id)
        if events is not None:
            return events[after_index:]
        # Cross-worker: read from DB
        return self._load_events_from_db(run_id, after_index)

    async def wait_for_run(self, run_id: str, timeout: float | None = None) -> AgentRun | None:
        waiter = _run_waiters.get(run_id)
        if not waiter:
            return None
        try:
            await asyncio.wait_for(waiter.wait(), timeout=timeout)
        except asyncio.TimeoutError:
            pass
        return _active_runs.get(run_id)

    async def stream_run_events(self, run_id: str, last_index: int = 0) -> AsyncIterator[dict]:
        """Yield events as they arrive. Cross-worker safe via DB polling."""
        idx = last_index
        max_idle_polls = 3000  # ~5 min at 0.1s interval
        idle_count = 0
        db_check_interval = 20  # Check DB every ~2s, not every 0.1s
        keepalive_interval = 150  # Send keepalive every ~15s (150 * 0.1s)

        while idle_count < max_idle_polls:
            # Re-check each iteration (run may be GC'd mid-stream)
            local = run_id in _run_events
            if local:
                events = _run_events.get(run_id, [])
            else:
                events = self._load_events_from_db(run_id, 0)

            if idx < len(events):
                for i in range(idx, len(events)):
                    yield events[i]
                idx = len(events)
                idle_count = 0  # Reset on activity
            else:
                idle_count += 1
                if idle_count % keepalive_interval == 0:
                    yield {"event_type": "keepalive", "data": {}}

            # Check if run is done (DB check only every db_check_interval polls)
            run = _active_runs.get(run_id)
            if run and run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                return
            if not run and idle_count % db_check_interval == 0:
                db_run = self.restore_run(run_id)
                if not db_run or db_run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                    return

            await asyncio.sleep(0.1)

    # ── Event persistence ─────────────────────────────────────

    def _append_event(self, run_id: str, sse: dict) -> None:
        """Append event to local buffer AND persist to DB.

        Individual INSERTs are issued immediately but COMMITs are deferred
        and batched — _flush_run_events() commits all pending writes at once.
        This is called automatically every _RUN_EVENT_FLUSH_SIZE events and
        in _complete_run / finally to ensure nothing is lost.
        """
        events = _run_events.setdefault(run_id, [])
        idx = len(events)
        events.append(sse)
        try:
            self.db.execute(
                text(
                    "INSERT INTO run_events (run_id, idx, event_type, data, event_id, agent_id) "
                    "VALUES (:run_id, :idx, :event_type, :data, :event_id, :agent_id)"
                ),
                {
                    "run_id": run_id,
                    "idx": idx,
                    "event_type": sse.get("event_type", ""),
                    "data": json.dumps(sse.get("data", {})),
                    "event_id": sse.get("event_id"),
                    "agent_id": sse.get("agent_id"),
                },
            )
            self._pending_event_count += 1
            if self._pending_event_count >= _RUN_EVENT_FLUSH_SIZE:
                self._flush_run_events()
        except Exception as e:
            self.db.rollback()
            # Reset counter: rollback discarded all pending INSERTs
            self._pending_event_count = 0
            logger.warning(f"Event persist failed for run {run_id} idx {idx}: {e}")

    def _flush_run_events(self) -> None:
        """Commit all pending run_event INSERTs in a single batch."""
        if self._pending_event_count <= 0:
            return
        try:
            self.db.commit()
        except Exception as e:
            self.db.rollback()
            logger.warning("Event batch commit failed: %s", e)
        self._pending_event_count = 0

    def _load_events_from_db(self, run_id: str, after_index: int = 0) -> list[dict]:
        """Load events from DB for cross-worker streaming."""
        try:
            rows = self.db.execute(
                text(
                    "SELECT event_type, data, event_id, agent_id FROM run_events "
                    "WHERE run_id = :run_id AND idx >= :after "
                    "ORDER BY idx"
                ),
                {"run_id": run_id, "after": after_index},
            ).fetchall()
            result = []
            for row in rows:
                data = row[1]
                if isinstance(data, str):
                    data = json.loads(data)
                result.append({
                    "event_type": row[0],
                    "data": data,
                    "event_id": row[2],
                    "agent_id": row[3],
                    "run_id": run_id,
                })
            return result
        except Exception as e:
            logger.error(f"Failed to load events from DB for {run_id}: {e}")
            return []

    # ── Distributed coordination ──────────────────────────────

    def _try_claim_resume(self, run_id: str) -> bool:
        """Optimistic lock: INSERT a unique claim row per resume attempt.

        Uses DB-derived counter so cross-worker resume cycles get unique idx
        values: -1, -2, -3, ... even when workers have no shared memory.
        UNIQUE(run_id, idx) ensures only one worker wins each cycle.
        """
        try:
            # Get next claim idx from DB (works across workers)
            row = self.db.execute(
                text("SELECT MIN(idx) FROM run_events "
                     "WHERE run_id = :run_id AND event_type = 'resume_claim'"),
                {"run_id": run_id},
            ).fetchone()
            prev_min = row[0] if row and row[0] is not None else 0
            claim_idx = prev_min - 1  # -1, -2, -3, ...

            self.db.execute(
                text(
                    "INSERT INTO run_events (run_id, idx, event_type, data) "
                    "VALUES (:run_id, :idx, 'resume_claim', :data)"
                ),
                {
                    "run_id": run_id,
                    "idx": claim_idx,
                    "data": json.dumps({"claimed_at": datetime.now(timezone.utc).isoformat()}),
                },
            )
            self.db.commit()
            return True
        except IntegrityError:
            self.db.rollback()
            return False
        except Exception as e:
            self.db.rollback()
            logger.error(f"Claim resume failed for {run_id}: {e}")
            return False  # Fail safe: reject on error in distributed mode

    def _is_cancelled_in_db(self, run_id: str) -> bool:
        try:
            row = self.db.execute(
                text(
                    "SELECT 1 FROM conversation_events "
                    "WHERE event_type = :et AND run_id = :run_id "
                    "LIMIT 1"
                ),
                {"et": EventType.RUN_CANCELLED.value, "run_id": run_id},
            ).fetchone()
            return row is not None
        except Exception:
            return False

    def _find_waiting_run_by_handle(self, handle: str) -> str | None:
        try:
            row = self.db.execute(
                text(
                    "SELECT run_id FROM conversation_events "
                    "WHERE event_type = :et AND waiting_for = :handle "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"et": EventType.RUN_WAITING.value, "handle": handle},
            ).fetchone()
            return row[0] if row else None
        except Exception as e:
            logger.error(f"DB lookup for handle {handle} failed: {e}")
            return None

    # ── Internal ──────────────────────────────────────────────

    def _complete_run(self, run: AgentRun) -> None:
        self._flush_run_events()  # Flush any remaining buffered events before marking complete
        run.status = RunStatus.COMPLETED
        run.completed_at = datetime.now(timezone.utc)
        self._log_run_event(run, EventType.RUN_COMPLETED)

        if run.parent_run_id:
            self._append_event(run.parent_run_id, {
                "event_type": "child_run_completed",
                "data": {"child_run_id": run.run_id},
                "run_id": run.parent_run_id,
            })

    async def _check_fan_in(self, parent_run_id: str) -> None:
        """If all child runs completed, resume the parent with aggregated results.

        Checks in-memory first, falls back to DB for cross-worker coordination.
        """
        children = _child_runs.get(parent_run_id)

        # Fallback: if no in-memory children, query DB
        if not children:
            children = self._get_child_run_ids_from_db(parent_run_id)
            if not children:
                return

        results = {}
        for cid in children:
            child = _active_runs.get(cid)
            if not child:
                # Single DB restore for both status and agent_id
                child = self.restore_run(cid)
            status = child.status if child else None
            if status not in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                return  # Still waiting for some children

            # Collect output — prefer text_done full_text, also gather tool_result
            child_events = _run_events.get(cid)
            if child_events is None:
                child_events = self._load_events_from_db(cid, 0)
            final_text = ""
            has_text_done = False
            tool_results = []
            for ev in child_events:
                et = ev.get("event_type", "")
                if et == "text_done":
                    # text_done carries the complete assembled text
                    final_text = ev.get("data", {}).get("full_text", final_text)
                    has_text_done = True
                elif et == "text_delta" and not has_text_done:
                    # Accumulate deltas only if no text_done seen yet
                    final_text += ev.get("data", {}).get("chunk", "")
                elif et == "tool_result":
                    tool_results.append(ev.get("data", {}))

            agent_id = child.agent_id if child else cid
            results[agent_id] = {
                "run_id": cid,
                "status": status.value if hasattr(status, 'value') else str(status),
                "output": final_text or "(no text output)",
                "tool_results": tool_results,
            }

        # All done — resume parent
        _child_runs.pop(parent_run_id, None)
        await self.resume_run(parent_run_id, {"child_results": results})

    def _get_child_run_ids_from_db(self, parent_run_id: str) -> set[str]:
        """Query DB for child run IDs of a parent."""
        try:
            rows = self.db.execute(
                text(
                    "SELECT DISTINCT run_id FROM conversation_events "
                    "WHERE event_type = :et "
                    "AND parent_run_id = :pid"
                ),
                {"et": EventType.RUN_STARTED.value, "pid": parent_run_id},
            ).fetchall()
            return {row[0] for row in rows if row[0]}
        except Exception as e:
            logger.warning(f"Failed to query child runs for {parent_run_id}: {e}")
            return set()

    def _get_run_status_from_db(self, run_id: str) -> RunStatus | None:
        """Get latest run status from DB."""
        restored = self.restore_run(run_id)
        return restored.status if restored else None

    def _get_agent_id_from_db(self, run_id: str) -> str:
        """Get agent_id for a run from DB. Falls back to run_id."""
        restored = self.restore_run(run_id)
        return restored.agent_id if restored else run_id

    _TERMINAL_EVENT_TYPES = {EventType.RUN_COMPLETED, EventType.RUN_FAILED, EventType.RUN_CANCELLED}

    def _log_run_event(self, run: AgentRun, event_type: EventType, extra_meta: dict | None = None) -> None:
        meta = {"run_id": run.run_id}
        if run.parent_run_id:
            meta["parent_run_id"] = run.parent_run_id
        if run.waiting_for:
            meta["waiting_for"] = run.waiting_for
        if extra_meta:
            meta.update(extra_meta)

        # Propagate causal chain from parent for audit traceability
        causal_chain_id = (run.context or {}).get("_causal_chain_id")

        self.event_logger.create_stream_event(
            user_id=run.user_id,
            session_id=run.session_id,
            event_type=event_type.value,
            content=run.to_event_content(),
            causal_chain_id=causal_chain_id,
            metadata=meta,
        )

        # Terminal states must be visible for cross-worker polling
        if event_type in self._TERMINAL_EVENT_TYPES:
            self.event_logger.flush_critical()

    def _write_cancel_event_for_run(self, run_id: str, session_id: str, user_id: str) -> bool:
        """Write a cancel event to DB for a run on another worker.

        Returns True if event was written successfully, False otherwise.
        """
        try:
            self.event_logger.create_stream_event(
                user_id=user_id,
                session_id=session_id,
                event_type=EventType.RUN_CANCELLED.value,
                content="{}",
                metadata={"run_id": run_id},
            )
            return True
        except Exception as e:
            logger.warning(f"Failed to write cross-worker cancel for {run_id}: {e}")
            return False

    @staticmethod
    def _maybe_gc() -> None:
        """Remove oldest completed runs from memory if over threshold."""
        terminal = {RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED}
        completed = [
            (rid, r) for rid, r in _active_runs.items()
            if r.status in terminal
        ]
        if len(completed) <= _MAX_COMPLETED_RUNS:
            return
        # Sort by completed_at, remove oldest
        completed.sort(key=lambda x: x[1].completed_at or x[1].created_at)
        to_remove = len(completed) - _MAX_COMPLETED_RUNS
        for rid, _ in completed[:to_remove]:
            _active_runs.pop(rid, None)
            _run_events.pop(rid, None)
            _run_waiters.pop(rid, None)

    @staticmethod
    def _stream_event_to_dict(event: StreamEvent, run_id: str) -> dict:
        return {
            "event_type": event.event_type.value if hasattr(event.event_type, 'value') else str(event.event_type),
            "data": event.data,
            "event_id": event.event_id,
            "causal_chain_id": event.causal_chain_id,
            "agent_id": event.agent_id,
            "run_id": run_id,
        }

    # ── State Recovery ────────────────────────────────────────

    def restore_run(self, run_id: str) -> AgentRun | None:
        """Restore run state from conversation_events."""
        rows = self.db.execute(
            text(
                "SELECT event_type, content, `metadata` FROM conversation_events "
                "WHERE JSON_UNQUOTE(JSON_EXTRACT(`metadata`, '$.run_id')) = :run_id "
                "ORDER BY created_at"
            ),
            {"run_id": run_id},
        ).fetchall()

        if not rows:
            return None

        run = None
        for row in rows:
            et = row[0]
            if et == EventType.RUN_STARTED.value:
                run = AgentRun.from_event_content(row[1])
            elif run and et == EventType.RUN_WAITING.value:
                meta = json.loads(row[2]) if isinstance(row[2], str) else row[2]
                run.status = RunStatus.WAITING
                run.waiting_for = meta.get("waiting_for")
            elif run and et == EventType.RUN_COMPLETED.value:
                run.status = RunStatus.COMPLETED
            elif run and et == EventType.RUN_FAILED.value:
                run.status = RunStatus.FAILED
            elif run and et == EventType.RUN_CANCELLED.value:
                run.status = RunStatus.CANCELLED

        return run
