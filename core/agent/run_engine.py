"""RunEngine — drives AgentRun execution, decoupled from HTTP lifecycle.

Distributed-safe: all coordination through DB, no cross-worker in-memory deps.
"""

import asyncio
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

logger = get_logger(__name__)

# In-memory: only for THIS worker's active runs (not shared across workers)
_active_runs: dict[str, AgentRun] = {}
_run_events: dict[str, list[dict]] = {}  # local buffer, also persisted to DB
_run_waiters: dict[str, asyncio.Event] = {}
_run_tasks: dict[str, asyncio.Task] = {}
_child_runs: dict[str, set[str]] = {}  # parent_run_id → {child_run_ids}


class RunEngine:
    """Drives AgentRun execution. Not bound to HTTP request lifecycle."""

    def __init__(self, db: Session):
        self.db = db
        self.event_logger = EventLogger(db)

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

        child = self.create_run(
            session_id=parent.session_id,
            user_id=parent.user_id,
            user_input=task,
            agent_id=agent_id,
            parent_run_id=parent_run_id,
            trigger=RunTrigger.USER_MESSAGE,
            context=context,
        )
        _child_runs.setdefault(parent_run_id, set()).add(child.run_id)

        # Start child in background
        task_obj = asyncio.create_task(self.start_run(child))
        _run_tasks[child.run_id] = task_obj
        return child

    async def start_run(self, run: AgentRun) -> None:
        """Execute an AgentRun using ChatLoop. Streams events to buffer."""
        run.status = RunStatus.RUNNING
        try:
            from api.routers.chat import _build_chat_loop
            loop = _build_chat_loop(self.db)
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
        except asyncio.CancelledError:
            run.status = RunStatus.CANCELLED
            self._log_run_event(run, EventType.RUN_CANCELLED)
        except Exception as e:
            logger.error(f"Run {run.run_id} failed: {e}", exc_info=True)
            run.status = RunStatus.FAILED
            self._log_run_event(run, EventType.RUN_FAILED, {"error": str(e)})
            self._append_event(run.run_id, {
                "event_type": "run_error", "data": {"error": str(e)},
                "run_id": run.run_id,
            })
        finally:
            _run_tasks.pop(run.run_id, None)
            _run_waiters.get(run.run_id, asyncio.Event()).set()

    async def _consume_stream(self, loop, run: AgentRun) -> None:
        """Consume ChatLoop stream, parking on wait_for signals."""
        async for event in loop.run_step_stream(
            user_input=run.user_input,
            session_id=run.session_id,
            user_id=run.user_id,
            context=run.context,
        ):
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

        import json as _json
        result_summary = _json.dumps(result, default=str)[:2000]
        run.user_input = (
            f"[Async result from {waiting_for}]:\n{result_summary}\n\n"
            f"Original task: {run.user_input}"
        )

        self._append_event(run.run_id, {
            "event_type": "tool_result",
            "data": {"result": result},
            "run_id": run.run_id,
        })
        await self.start_run(run)

    def cancel_run(self, run_id: str) -> bool:
        run = _active_runs.get(run_id)
        if not run or run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
            return False
        run.status = RunStatus.CANCELLED
        self._log_run_event(run, EventType.RUN_CANCELLED)

        # Cancel the asyncio task
        task = _run_tasks.pop(run_id, None)
        if task and not task.done():
            task.cancel()

        # Propagate to workflow
        if run.waiting_for and run.waiting_for.startswith("workflow:"):
            wf_id = run.waiting_for.split(":", 1)[1]
            self._cancel_workflow(wf_id)

        _run_waiters.get(run_id, asyncio.Event()).set()
        return True

    @staticmethod
    def _cancel_workflow(wf_id: str) -> None:
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
            from api.database import get_db_session
            from api.models import WorkflowRun as WFRunModel
            db = next(get_db_session())
            try:
                db.execute(
                    text("UPDATE workflow_runs SET status='cancelled', error='Cancelled by user' "
                         "WHERE run_id = :wf_id AND status IN ('running','waiting')"),
                    {"wf_id": wf_id},
                )
                db.commit()
            finally:
                db.close()
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
        local = run_id in _run_events  # Is this run on this worker?

        while True:
            if local:
                events = _run_events.get(run_id, [])
            else:
                events = self._load_events_from_db(run_id, 0)

            if idx < len(events):
                for i in range(idx, len(events)):
                    yield events[i]
                idx = len(events)

            # Check if run is done
            run = _active_runs.get(run_id)
            if run and run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                return
            if not run:
                # Cross-worker: check DB for terminal status
                db_run = self.restore_run(run_id)
                if db_run and db_run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                    return
                if not db_run:
                    return

            await asyncio.sleep(0.1)

    # ── Event persistence ─────────────────────────────────────

    def _append_event(self, run_id: str, sse: dict) -> None:
        """Append event to local buffer AND persist to DB."""
        events = _run_events.setdefault(run_id, [])
        idx = len(events)
        events.append(sse)
        # Persist to run_events table
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
            self.db.commit()
        except Exception as e:
            logger.debug(f"Event persist failed (non-fatal): {e}")

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
        """Optimistic lock: INSERT a unique claim row.

        The run_events table has a unique index on (run_id, idx) where idx=-1
        is reserved for resume claims. Second INSERT raises IntegrityError.
        """
        try:
            self.db.execute(
                text(
                    "INSERT INTO run_events (run_id, idx, event_type, data) "
                    "VALUES (:run_id, -1, 'resume_claim', :data)"
                ),
                {"run_id": run_id, "data": json.dumps({"claimed_at": datetime.now(timezone.utc).isoformat()})},
            )
            self.db.commit()
            return True
        except IntegrityError:
            self.db.rollback()
            return False
        except Exception as e:
            logger.debug(f"Claim resume failed for {run_id}: {e}")
            return True  # On error, allow resume (single-worker fallback)

    def _is_cancelled_in_db(self, run_id: str) -> bool:
        try:
            row = self.db.execute(
                text(
                    "SELECT 1 FROM conversation_events "
                    "WHERE event_type = :et AND JSON_EXTRACT(metadata, '$.run_id') = :run_id "
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
                    "SELECT JSON_EXTRACT(metadata, '$.run_id') FROM conversation_events "
                    "WHERE event_type = :et AND JSON_EXTRACT(metadata, '$.waiting_for') = :handle "
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
        run.status = RunStatus.COMPLETED
        run.completed_at = datetime.now(timezone.utc)
        self._log_run_event(run, EventType.RUN_COMPLETED)

        if run.parent_run_id:
            self._append_event(run.parent_run_id, {
                "event_type": "child_run_completed",
                "data": {"child_run_id": run.run_id},
                "run_id": run.parent_run_id,
            })
            # Fan-in: check if all siblings are done
            asyncio.ensure_future(self._check_fan_in(run.parent_run_id))

    async def _check_fan_in(self, parent_run_id: str) -> None:
        """If all child runs completed, resume the parent with aggregated results."""
        children = _child_runs.get(parent_run_id)
        if not children:
            return

        results = {}
        for cid in children:
            child = _active_runs.get(cid)
            if not child or child.status not in (RunStatus.COMPLETED, RunStatus.FAILED):
                return  # Still waiting for some children
            results[cid] = {
                "agent_id": child.agent_id,
                "status": child.status.value,
                "events": _run_events.get(cid, []),
            }

        # All done — resume parent
        _child_runs.pop(parent_run_id, None)
        handle = f"children:{parent_run_id}"
        await self.resume_run(parent_run_id, {"child_results": results})

    def _log_run_event(self, run: AgentRun, event_type: EventType, extra_meta: dict | None = None) -> None:
        meta = {"run_id": run.run_id}
        if run.parent_run_id:
            meta["parent_run_id"] = run.parent_run_id
        if run.waiting_for:
            meta["waiting_for"] = run.waiting_for
        if extra_meta:
            meta.update(extra_meta)

        self.event_logger.create_stream_event(
            user_id=run.user_id,
            session_id=run.session_id,
            event_type=event_type.value,
            content=run.to_event_content(),
            metadata=meta,
        )

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
                "SELECT event_type, content, metadata FROM conversation_events "
                "WHERE JSON_EXTRACT(metadata, '$.run_id') = :run_id "
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
