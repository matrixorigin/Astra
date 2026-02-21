"""RunEngine — drives AgentRun execution, decoupled from HTTP lifecycle."""

import asyncio
import json
from collections.abc import AsyncIterator
from datetime import datetime, timezone

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.agent.run import AgentRun, RunStatus, RunTrigger
from core.events.event_logger import EventLogger
from core.events.models import EventType, StreamEvent, StreamEventType
from core.logging_config import get_logger

logger = get_logger(__name__)

# In-memory registry of active runs (production would use Redis)
_active_runs: dict[str, AgentRun] = {}
_run_events: dict[str, list[dict]] = {}  # run_id → buffered SSE events
_run_waiters: dict[str, asyncio.Event] = {}  # run_id → completion signal


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

    async def start_run(self, run: AgentRun) -> None:
        """Execute an AgentRun using ChatLoop. Streams events to buffer."""
        run.status = RunStatus.RUNNING
        try:
            from api.routers.chat import _build_chat_loop
            loop = _build_chat_loop(self.db)
            loop._current_run_id = run.run_id  # For async tools to link jobs

            async for event in loop.run_step_stream(
                user_input=run.user_input,
                session_id=run.session_id,
                user_id=run.user_id,
                context=run.context,
            ):
                sse = self._stream_event_to_dict(event, run.run_id)
                _run_events.setdefault(run.run_id, []).append(sse)

                # Check for async wait signal
                if event.data.get("wait_for"):
                    run.status = RunStatus.WAITING
                    run.waiting_for = event.data["wait_for"]
                    self._log_run_event(run, EventType.RUN_WAITING, {
                        "waiting_for": run.waiting_for,
                    })
                    return  # Park the run

            self._complete_run(run)
        except asyncio.CancelledError:
            run.status = RunStatus.CANCELLED
            self._log_run_event(run, EventType.RUN_CANCELLED)
        except Exception as e:
            logger.error(f"Run {run.run_id} failed: {e}", exc_info=True)
            run.status = RunStatus.FAILED
            self._log_run_event(run, EventType.RUN_FAILED, {"error": str(e)})
            _run_events.setdefault(run.run_id, []).append({
                "event_type": "run_error", "data": {"error": str(e)},
                "run_id": run.run_id,
            })
        finally:
            _run_waiters.get(run.run_id, asyncio.Event()).set()

    async def resume_run(self, run_id: str, result: dict) -> None:
        """Resume a waiting run when its async event arrives."""
        run = _active_runs.get(run_id)
        if not run or run.status != RunStatus.WAITING:
            logger.warning(f"Cannot resume run {run_id}: not waiting")
            return

        run.status = RunStatus.RUNNING
        waiting_for = run.waiting_for
        run.waiting_for = None
        self._log_run_event(run, EventType.RUN_RESUMED, {"result": result})

        # Inject result into context so agent sees it on next LLM call
        run.context = run.context or {}
        run.context["resumed_from"] = waiting_for
        run.context["async_result"] = result

        # Prepend result to user_input so LLM sees what happened
        import json as _json
        result_summary = _json.dumps(result, default=str)[:2000]
        run.user_input = (
            f"[Async result from {waiting_for}]:\n{result_summary}\n\n"
            f"Original task: {run.user_input}"
        )

        _run_events.setdefault(run.run_id, []).append({
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
        _run_waiters.get(run_id, asyncio.Event()).set()
        return True

    async def on_job_completed(self, job_id: str, result: dict) -> bool:
        """Called when a background job completes. Resumes the waiting run."""
        return await self.resolve_handle(f"job:{job_id}", {"job_id": job_id, **result})

    async def resolve_handle(self, handle: str, result: dict) -> bool:
        """Resolve any wait handle. Resumes the run waiting for it."""
        from core.agent.async_tools import get_async_tool_registry
        run_id = get_async_tool_registry().resolve_handle(handle)
        if not run_id:
            logger.warning(f"No run waiting for handle {handle}")
            return False
        await self.resume_run(run_id, result)
        return True

    def get_run(self, run_id: str) -> AgentRun | None:
        return _active_runs.get(run_id)

    def get_run_events(self, run_id: str, after_index: int = 0) -> list[dict]:
        events = _run_events.get(run_id, [])
        return events[after_index:]

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
        """Yield events as they arrive. Supports reconnection via last_index."""
        # Replay buffered events
        events = _run_events.get(run_id, [])
        for i in range(last_index, len(events)):
            yield events[i]

        # Live stream
        idx = len(events)
        while True:
            run = _active_runs.get(run_id)
            if not run:
                return

            current_events = _run_events.get(run_id, [])
            if idx < len(current_events):
                for i in range(idx, len(current_events)):
                    yield current_events[i]
                idx = len(current_events)
            elif run.status in (RunStatus.COMPLETED, RunStatus.FAILED, RunStatus.CANCELLED):
                return
            else:
                await asyncio.sleep(0.05)

    # ── Internal ──────────────────────────────────────────────

    def _complete_run(self, run: AgentRun) -> None:
        run.status = RunStatus.COMPLETED
        run.completed_at = datetime.now(timezone.utc)
        self._log_run_event(run, EventType.RUN_COMPLETED)

        # If this is a child run, notify parent
        if run.parent_run_id:
            _run_events.setdefault(run.parent_run_id, []).append({
                "event_type": "child_run_completed",
                "data": {"child_run_id": run.run_id},
                "run_id": run.parent_run_id,
            })

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

    # ── State Recovery (Phase 1: from in-memory; Phase 2: from DB) ──

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
