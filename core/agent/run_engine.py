"""RunEngine — drives AgentRun execution, decoupled from HTTP lifecycle.

Distributed-safe: all coordination through DB, no cross-worker in-memory deps.
"""

import asyncio
import gc
import json
from collections.abc import AsyncIterator
from datetime import datetime, timezone

from sqlalchemy import text
from sqlalchemy.exc import IntegrityError

from core.agent.run import AgentRun, RunStatus, RunTrigger
from core.db_consumer import DbConsumer
from core.events.event_logger import EventLogger
from core.events.models import EventType, StreamEvent, StreamEventType
from core.logging_config import get_logger

logger = get_logger(__name__)

# In-memory: only for THIS worker's active runs (not shared across workers)
_active_runs: dict[str, AgentRun] = {}
_agent_run_events: dict[str, list[dict]] = {}  # local buffer, also persisted to DB
_run_waiters: dict[str, asyncio.Event] = {}
_run_tasks: dict[str, asyncio.Task] = {}
_child_runs: dict[str, set[str]] = {}  # parent_run_id → {child_run_ids}
_run_notifiers: dict[str, asyncio.Event] = {}  # wake stream_agent_run_events
_fan_in_tasks: set[asyncio.Task] = set()  # Track fan-in tasks for cleanup
_cancel_pending: set[asyncio.Task] = set()  # Hold refs to cancelled tasks until done


def cleanup_fan_in_tasks() -> None:
    """Cancel all pending fan-in tasks. Call during shutdown or test teardown."""
    for t in list(_fan_in_tasks):
        try:
            if not t.done():
                t.cancel()
        except RuntimeError:
            pass  # event loop already closed
    _fan_in_tasks.clear()


def cleanup_run_tasks() -> None:
    """Cancel all pending run tasks. Call during test teardown."""
    import asyncio

    tasks_to_cancel = [t for t in _run_tasks.values() if not t.done()]
    
    for t in tasks_to_cancel:
        try:
            t.cancel()
        except RuntimeError:
            # Event loop may be closed
            pass

    # Try to let cancelled tasks finish
    if tasks_to_cancel:
        try:
            loop = asyncio.get_event_loop()
            if not loop.is_closed() and not loop.is_running():
                loop.run_until_complete(
                    asyncio.gather(*tasks_to_cancel, return_exceptions=True)
                )
        except Exception:
            pass

    _run_tasks.clear()
    _active_runs.clear()
    _agent_run_events.clear()
    _run_waiters.clear()
    _run_notifiers.clear()
    _child_runs.clear()
    _cancel_pending.clear()


# Max size for resume user_input to prevent token explosion on adversarial loops
_MAX_RESUME_INPUT_CHARS = 4000
# Max completed runs to keep in memory before cleanup
_MAX_COMPLETED_RUNS = 500
# Periodic GC interval in seconds
_GC_INTERVAL_SECONDS = 300
# Batch flush threshold for agent_run_events (streaming events)
_RUN_EVENT_FLUSH_SIZE = 20
# Hard cap on pending inserts to prevent unbounded memory growth during DB outages.
_MAX_PENDING_EVENTS = 500

# Global GC task reference
_gc_task: asyncio.Task | None = None


async def _periodic_gc() -> None:
    """Clean up completed runs periodically to prevent memory leaks."""
    while True:
        try:
            await asyncio.sleep(_GC_INTERVAL_SECONDS)
            RunEngine._maybe_gc()
            gc.collect()
            logger.debug(f"Periodic GC: {len(_active_runs)} active runs, {len(_agent_run_events)} event buffers")
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


class RunEngine(DbConsumer):
    """Drives AgentRun execution. Not bound to HTTP request lifecycle.

    Accepts a db_factory (Callable → Session) instead of a long-lived session.
    Each DB operation acquires a short-lived session via ``with self._db()``.
    """

    def __init__(self, db_factory, chat_loop_factory=None):
        """
        Args:
            db_factory: Callable that returns a new SQLAlchemy Session.
            chat_loop_factory: Callable(db_factory) -> ChatLoop.
        """
        super().__init__(db_factory)
        self._chat_loop_factory = chat_loop_factory
        self._pending_inserts: list[dict] = []
        # Run lifecycle EventLogger — no pipeline, always synchronous writes.
        # Shared across _log_run_event / _write_cancel_event_for_run calls.
        self._run_event_logger = EventLogger(db_factory)
        _start_gc_task()

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
        _agent_run_events[run.run_id] = []
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
        """Load system_prompt from agent_agents table config."""
        config = self._load_agent_config(agent_id)
        return config.get("system_prompt") if config else None

    def _load_agent_config(self, agent_id: str) -> dict | None:
        """Load agent_config from agent_agents table."""
        try:
            with self._db() as db:
                from api.models import Agent
                row = db.query(Agent.agent_config).filter(Agent.agent_id == agent_id).first()
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

        No long-lived DB session is held. Each DB operation acquires a
        short-lived session from the factory and releases it immediately.
        """
        run.status = RunStatus.RUNNING
        loop = None
        try:
            # Load agent config and inject model if not already set
            if run.agent_id:
                run.context = run.context or {}
                self._apply_agent_config(run.agent_id, run.context)

            # Build ChatLoop — pass db_factory so it also uses short-lived sessions
            factory = getattr(self, '_chat_loop_factory', None)
            if factory:
                loop = factory(self._db_factory)
            else:
                from api.routers.chat import _build_chat_loop
                loop = _build_chat_loop(self._db_factory)
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
            raise
        except asyncio.CancelledError:
            run.status = RunStatus.CANCELLED
            run._cancelled_externally = True
            self._log_run_event(run, EventType.RUN_CANCELLED)
            raise
        except Exception as e:
            logger.error(f"Run {run.run_id} failed: {e}", exc_info=True)
            run.status = RunStatus.FAILED
            self._log_run_event(run, EventType.RUN_FAILED, {"error": str(e)})
            self._append_event(run.run_id, {
                "event_type": "run_error", "data": {"error": str(e)},
                "run_id": run.run_id,
            })
            raise
        finally:
            self._flush_agent_run_events()
            # Shutdown EventPipeline
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
            # Wait for GateTrigger daemon threads (run in executor to avoid blocking the event loop)
            try:
                gt = getattr(loop, '_gate_trigger', None) if loop else None
                if gt and hasattr(gt, 'wait_all'):
                    await asyncio.get_running_loop().run_in_executor(None, gt.wait_all, 5.0)
            except Exception:
                pass
            _run_tasks.pop(run.run_id, None)
            _run_waiters.get(run.run_id, asyncio.Event()).set()
            if run.parent_run_id and run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED) \
                    and not getattr(run, '_cancelled_externally', False):
                try:
                    await self._check_fan_in(run.parent_run_id)
                except Exception:
                    pass
            if run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                self._maybe_gc()

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
                _agent_run_events.setdefault(run_id, [])
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

        # Cancel the asyncio task (keep ref in _cancel_pending until done
        # to prevent "Task was destroyed but it is pending!" warnings)
        task = _run_tasks.pop(run_id, None)
        if task and not task.done():
            task.cancel()
            _cancel_pending.add(task)
            task.add_done_callback(_cancel_pending.discard)

        # Cancel children (local + write DB for cross-worker)
        children = _child_runs.pop(run_id, set())
        # Also check DB for children on other workers
        if not children:
            children = self._get_child_run_ids_from_db(run_id)
        for cid in children:
            child_task = _run_tasks.pop(cid, None)
            if child_task and not child_task.done():
                child_task.cancel()
                _cancel_pending.add(child_task)
                child_task.add_done_callback(_cancel_pending.discard)
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
        from core.agent.async_tools import _wf_runs, _workflow_waits
        entry = _wf_runs.pop(wf_id, None)
        if entry and entry.get("engine"):
            entry["engine"].cancel(entry["workflow"].name)
        to_remove = [h for h, wid in _workflow_waits.items() if wid == wf_id]
        for h in to_remove:
            _workflow_waits.pop(h, None)
        try:
            with self._db() as db:
                db.execute(
                    text("UPDATE wf_runs SET status='cancelled', error='Cancelled by user' "
                         "WHERE run_id = :wf_id AND status IN ('running','waiting')"),
                    {"wf_id": wf_id},
                )
                db.commit()
        except Exception as e:
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

    def get_agent_run_events(self, run_id: str, after_index: int = 0) -> list[dict]:
        """Get events — local buffer first, DB fallback for cross-worker."""
        events = _agent_run_events.get(run_id)
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

    async def stream_agent_run_events(self, run_id: str, last_index: int = 0) -> AsyncIterator[dict]:
        """Yield events as they arrive. Cross-worker safe via DB polling."""
        idx = last_index
        local = run_id in _agent_run_events
        db_poll_interval = 1.0  # Cross-worker: poll DB every 1s
        max_idle_s = 300.0  # 5 min timeout
        keepalive_s = 15.0
        elapsed_idle = 0.0
        since_keepalive = 0.0

        # Local mode: use asyncio.Event for instant wake-up, no polling
        notifier: asyncio.Event | None = None
        if local:
            notifier = _run_notifiers.setdefault(run_id, asyncio.Event())

        try:
            while elapsed_idle < max_idle_s:
                if local:
                    events = _agent_run_events.get(run_id, [])
                else:
                    events = self._load_events_from_db(run_id, 0)

                if idx < len(events):
                    for i in range(idx, len(events)):
                        yield events[i]
                    idx = len(events)
                    elapsed_idle = 0.0
                    since_keepalive = 0.0

                # Check if run is done
                run = _active_runs.get(run_id)
                if run and run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                    return
                if not run and elapsed_idle >= 2.0 and int(elapsed_idle) % 2 == 0:
                    db_run = self.restore_run(run_id)
                    if not db_run or db_run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                        return

                # Wait: event-driven for local, interval for cross-worker
                if notifier:
                    notifier.clear()
                    try:
                        await asyncio.wait_for(notifier.wait(), timeout=keepalive_s)
                        since_keepalive = 0.0
                    except asyncio.TimeoutError:
                        elapsed_idle += keepalive_s
                        since_keepalive = 0.0
                        yield {"event_type": "keepalive", "data": {}}
                else:
                    await asyncio.sleep(db_poll_interval)
                    elapsed_idle += db_poll_interval
                    since_keepalive += db_poll_interval
                    if since_keepalive >= keepalive_s:
                        since_keepalive = 0.0
                        yield {"event_type": "keepalive", "data": {}}
        finally:
            _run_notifiers.pop(run_id, None)

    # ── Event persistence ─────────────────────────────────────

    def _append_event(self, run_id: str, sse: dict) -> None:
        """Append event to local buffer AND queue for DB persistence.

        Events are buffered in memory. Every _RUN_EVENT_FLUSH_SIZE events,
        a short-lived session is acquired, all pending INSERTs are executed,
        committed, and the session is released.
        """
        events = _agent_run_events.setdefault(run_id, [])
        idx = len(events)
        events.append(sse)
        self._pending_inserts.append({
            "run_id": run_id,
            "idx": idx,
            "event_type": sse.get("event_type", ""),
            "data": json.dumps(sse.get("data", {})),
            "event_id": sse.get("event_id"),
            "agent_id": sse.get("agent_id"),
        })
        if len(self._pending_inserts) >= _RUN_EVENT_FLUSH_SIZE:
            self._flush_agent_run_events()

        # Wake up any local stream_agent_run_events waiters
        notifier = _run_notifiers.get(run_id)
        if notifier:
            notifier.set()

    def _flush_agent_run_events(self, *, _retried: bool = False) -> None:
        """Commit all pending run_event INSERTs in a single batch.

        On failure, retries once with a fresh session (handles transient
        connection errors).  If the retry also fails, the events are dropped
        and an error is logged — there is no further caller to retry.
        """
        if not self._pending_inserts:
            return
        batch = self._pending_inserts
        self._pending_inserts = []
        try:
            with self._db() as db:
                db.execute(
                    text(
                        "INSERT INTO agent_run_events (run_id, idx, event_type, data, event_id, agent_id) "
                        "VALUES (:run_id, :idx, :event_type, :data, :event_id, :agent_id)"
                    ),
                    batch,
                )
                db.commit()
        except Exception as e:
            if not _retried:
                logger.warning("Event batch commit failed, retrying with fresh session: %s", e)
                merged = batch + self._pending_inserts
                # Cap to prevent unbounded growth during persistent DB outages.
                if len(merged) > _MAX_PENDING_EVENTS:
                    dropped = len(merged) - _MAX_PENDING_EVENTS
                    merged = merged[-_MAX_PENDING_EVENTS:]
                    logger.warning("Pending event buffer capped: %d oldest events dropped", dropped)
                self._pending_inserts = merged
                self._flush_agent_run_events(_retried=True)
            else:
                logger.error("Event batch commit failed after retry, %d events dropped: %s", len(batch), e)

    def _load_events_from_db(self, run_id: str, after_index: int = 0) -> list[dict]:
        """Load events from DB for cross-worker streaming."""
        try:
            with self._db() as db:
                rows = db.execute(
                    text(
                        "SELECT event_type, data, event_id, agent_id FROM agent_run_events "
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

        Uses a single session for the SELECT + INSERT to preserve CAS semantics.
        """
        with self._db() as db:
            try:
                row = db.execute(
                    text("SELECT MIN(idx) FROM agent_run_events "
                         "WHERE run_id = :run_id AND event_type = 'resume_claim'"),
                    {"run_id": run_id},
                ).fetchone()
                prev_min = row[0] if row and row[0] is not None else 0
                claim_idx = prev_min - 1

                db.execute(
                    text(
                        "INSERT INTO agent_run_events (run_id, idx, event_type, data) "
                        "VALUES (:run_id, :idx, 'resume_claim', :data)"
                    ),
                    {
                        "run_id": run_id,
                        "idx": claim_idx,
                        "data": json.dumps({"claimed_at": datetime.now(timezone.utc).isoformat()}),
                    },
                )
                db.commit()
                return True
            except IntegrityError:
                # Explicit rollback required: we swallow the exception (return False),
                # so DbConsumer._db()'s except clause never fires.
                db.rollback()
                return False
            except Exception as e:
                db.rollback()
                logger.error(f"Claim resume failed for {run_id}: {e}")
                return False

    def _is_cancelled_in_db(self, run_id: str) -> bool:
        try:
            with self._db() as db:
                row = db.execute(
                    text(
                        "SELECT 1 FROM agent_events "
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
            with self._db() as db:
                row = db.execute(
                    text(
                        "SELECT run_id FROM agent_events "
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
        self._flush_agent_run_events()  # Flush any remaining buffered events before marking complete
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
            child_events = _agent_run_events.get(cid)
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
            with self._db() as db:
                rows = db.execute(
                    text(
                        "SELECT DISTINCT run_id FROM agent_events "
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

    def _log_run_event(self, run: AgentRun, event_type: EventType, extra_meta: dict | None = None) -> None:
        meta = {"run_id": run.run_id}
        if run.parent_run_id:
            meta["parent_run_id"] = run.parent_run_id
        if run.waiting_for:
            meta["waiting_for"] = run.waiting_for
        if extra_meta:
            meta.update(extra_meta)

        causal_chain_id = (run.context or {}).get("_causal_chain_id")

        # Run lifecycle events are always written synchronously (no pipeline).
        # Uses the cached self._run_event_logger which acquires a short-lived
        # session per log_event() call via DbConsumer._db().
        self._run_event_logger.create_stream_event(
            user_id=run.user_id,
            session_id=run.session_id,
            event_type=event_type.value,
            content=run.to_event_content(),
            causal_chain_id=causal_chain_id,
            metadata=meta,
        )

    def _write_cancel_event_for_run(self, run_id: str, session_id: str, user_id: str) -> bool:
        """Write a cancel event to DB for a run on another worker."""
        try:
            self._run_event_logger.create_stream_event(
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
            _agent_run_events.pop(rid, None)
            _run_waiters.pop(rid, None)
            _run_notifiers.pop(rid, None)

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
        """Restore run state from agent_events."""
        try:
            with self._db() as db:
                rows = db.execute(
                    text(
                        "SELECT event_type, content, `metadata` FROM agent_events "
                        "WHERE run_id = :run_id "
                        "ORDER BY created_at"
                    ),
                    {"run_id": run_id},
                ).fetchall()
        except Exception as e:
            logger.error(f"Failed to restore run {run_id}: {e}")
            return None

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
