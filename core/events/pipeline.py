"""Async event pipeline for high-throughput event ingestion.

Hot path: emit() enqueues in-memory, returns immediately (<1μs).
Background: drain → classify → batch INSERT → single COMMIT.
Embedding: completely decoupled — not in this pipeline.
"""

import asyncio
import atexit
import logging
import threading
from collections.abc import Callable
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.events.models import ConversationEvent, EventType

logger = logging.getLogger(__name__)

# Event tier classification
CRITICAL_TYPES = {
    EventType.USER_QUERY,
    EventType.LLM_RESPONSE,
}

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
    EventType.TOOL_CALL,
    EventType.TOOL_RESULT,
}


def _to_ce_values(e: ConversationEvent) -> dict[str, Any]:
    """Convert event to conversation_events column dict (no embedding)."""
    metadata = e.metadata or {}
    return {
        "event_id": e.event_id,
        "user_id": e.user_id,
        "session_id": e.session_id,
        "agent_id": e.agent_id,
        "agent_version": e.agent_version,
        "event_type": e.event_type if isinstance(e.event_type, str) else e.event_type.value,
        "content": e.content,
        "parent_event_id": e.parent_event_id,
        "causal_chain_id": e.causal_chain_id,
        "created_at": e.created_at,
        "metadata": metadata if metadata else None,
        "token_usage": e.token_usage.model_dump() if e.token_usage else None,
        "context_snapshot": e.context_snapshot.model_dump() if e.context_snapshot else None,
        "llm_model_used": e.llm_model_used,
        "llm_params": e.llm_params,
        "run_id": metadata.get("run_id"),
        "parent_run_id": metadata.get("parent_run_id"),
        "waiting_for": metadata.get("waiting_for"),
    }


def _to_re_values(e: ConversationEvent, run_id: str, idx: int) -> dict[str, Any]:
    """Convert event to run_events column dict."""
    et = e.event_type if isinstance(e.event_type, str) else e.event_type.value
    return {
        "run_id": run_id,
        "idx": idx,
        "event_type": et,
        "data": {"content": e.content, "metadata": e.metadata},
        "event_id": e.event_id,
        "agent_id": e.agent_id,
    }


# SQL templates for bulk insert
_CE_INSERT = text("""
    INSERT INTO conversation_events (
        event_id, user_id, session_id, agent_id, agent_version,
        event_type, content, parent_event_id, causal_chain_id,
        created_at, metadata, token_usage, context_snapshot,
        llm_model_used, llm_params, run_id, parent_run_id, waiting_for
    ) VALUES (
        :event_id, :user_id, :session_id, :agent_id, :agent_version,
        :event_type, :content, :parent_event_id, :causal_chain_id,
        :created_at, :metadata, :token_usage, :context_snapshot,
        :llm_model_used, :llm_params, :run_id, :parent_run_id, :waiting_for
    )
""")

_RE_INSERT = text("""
    INSERT INTO run_events (run_id, idx, event_type, data, event_id, agent_id)
    VALUES (:run_id, :idx, :event_type, :data, :event_id, :agent_id)
""")

# Backpressure thresholds
_WARN_THRESHOLD = 10_000
_DROP_EPHEMERAL_THRESHOLD = 100_000


class EventPipeline:
    """Async event ingestion pipeline.

    Hot path: emit() enqueues in-memory, returns immediately.
    Background: drain → classify → batch → flush to DB.
    """

    FLUSH_INTERVAL_S = 0.2  # 200ms
    FLUSH_BATCH_SIZE = 50

    def __init__(self, db_factory: Callable[[], Session]) -> None:
        self._db_factory = db_factory
        self._queue: asyncio.Queue[ConversationEvent] = asyncio.Queue()
        self._flush_task: asyncio.Task | None = None
        self._closed = False
        self._run_event_counters: dict[str, int] = {}  # run_id → next idx
        self._lock = threading.Lock()

        # Stats
        self.stats = {"emitted": 0, "flushed": 0, "dropped": 0}

        # Register shutdown
        self._atexit_registered = True
        atexit.register(self._atexit_flush)

    def start(self) -> None:
        """Start the background flush loop. Call from async context."""
        if self._flush_task is None or self._flush_task.done():
            try:
                loop = asyncio.get_running_loop()
                self._flush_task = loop.create_task(self._flush_loop())
            except RuntimeError:
                pass  # No running event loop — flush_loop will not run, sync flush still works

    def emit(self, event: ConversationEvent) -> str:
        """Fire-and-forget enqueue. Returns event_id immediately."""
        if self._closed:
            raise RuntimeError("EventPipeline is closed")

        qsize = self._queue.qsize()
        if qsize >= _DROP_EPHEMERAL_THRESHOLD:
            et = event.event_type if isinstance(event.event_type, str) else event.event_type.value
            try:
                et_enum = EventType(et)
            except ValueError:
                et_enum = None
            if et_enum not in CRITICAL_TYPES and et_enum not in DURABLE_TYPES:
                self.stats["dropped"] += 1
                return event.event_id
        elif qsize >= _WARN_THRESHOLD and qsize % 1000 == 0:
            logger.warning("EventPipeline queue depth: %d", qsize)

        self._queue.put_nowait(event)
        self.stats["emitted"] += 1
        return event.event_id

    def flush_critical(self) -> None:
        """Synchronous flush of all CRITICAL events currently in queue.

        Drains the queue, flushes critical events immediately,
        re-enqueues non-critical events.
        """
        critical = []
        requeue = []

        # Drain everything
        while not self._queue.empty():
            try:
                ev = self._queue.get_nowait()
            except asyncio.QueueEmpty:
                break
            et = ev.event_type if isinstance(ev.event_type, str) else ev.event_type.value
            try:
                et_enum = EventType(et)
            except ValueError:
                et_enum = None
            if et_enum in CRITICAL_TYPES:
                critical.append(ev)
            else:
                requeue.append(ev)

        # Re-enqueue non-critical
        for ev in requeue:
            self._queue.put_nowait(ev)

        if not critical:
            return

        # Flush critical synchronously
        db = self._db_factory()
        try:
            self._do_flush(db, critical)
            self.stats["flushed"] += len(critical)
        except Exception:
            db.rollback()
            logger.exception("flush_critical failed (%d events)", len(critical))
            self.stats["dropped"] += len(critical)
        finally:
            db.close()

    async def _flush_loop(self) -> None:
        """Background loop: drain → classify → batch INSERT → commit."""
        db = self._db_factory()
        try:
            while not self._closed:
                batch = await self._drain()
                if batch:
                    try:
                        self._do_flush(db, batch)
                        self.stats["flushed"] += len(batch)
                    except Exception:
                        db.rollback()
                        logger.warning("Background flush failed (%d events), retrying once", len(batch))
                        try:
                            self._do_flush(db, batch)
                            self.stats["flushed"] += len(batch)
                        except Exception:
                            db.rollback()
                            logger.exception("Background flush retry failed (%d events dropped)", len(batch))
                            self.stats["dropped"] += len(batch)

            # _closed is True — drain any remaining events before exiting
            remaining: list[ConversationEvent] = []
            while not self._queue.empty():
                try:
                    item = self._queue.get_nowait()
                    if item is not self._SENTINEL and isinstance(item, ConversationEvent):
                        remaining.append(item)
                except asyncio.QueueEmpty:
                    break
            if remaining:
                try:
                    self._do_flush(db, remaining)
                    self.stats["flushed"] += len(remaining)
                except Exception:
                    db.rollback()
                    self.stats["dropped"] += len(remaining)
        except asyncio.CancelledError:
            pass  # Shutdown requested — exit cleanly
        finally:
            db.close()

    async def _drain(self) -> list[ConversationEvent]:
        """Drain up to FLUSH_BATCH_SIZE events, waiting up to FLUSH_INTERVAL_S."""
        batch: list[ConversationEvent] = []
        try:
            # Wait for first event (with timeout)
            ev = await asyncio.wait_for(self._queue.get(), timeout=self.FLUSH_INTERVAL_S)
            if ev is self._SENTINEL:
                return batch  # Shutdown signal
            batch.append(ev)
        except (asyncio.TimeoutError, TimeoutError):
            return batch
        except asyncio.CancelledError:
            return batch  # Task cancelled during shutdown

        # Grab more without waiting
        while len(batch) < self.FLUSH_BATCH_SIZE and not self._queue.empty():
            try:
                item = self._queue.get_nowait()
                if item is self._SENTINEL:
                    break
                batch.append(item)
            except asyncio.QueueEmpty:
                break
        return batch

    def _do_flush(self, db: Session, events: list[ConversationEvent]) -> None:
        """Classify events, bulk INSERT into appropriate tables, single COMMIT."""
        ce_rows = []
        re_rows = []

        for ev in events:
            et = ev.event_type if isinstance(ev.event_type, str) else ev.event_type.value
            try:
                et_enum = EventType(et)
            except ValueError:
                et_enum = None

            # conversation_events: critical + durable
            if et_enum in CRITICAL_TYPES or et_enum in DURABLE_TYPES:
                ce_rows.append(_to_ce_values(ev))

            # run_events: anything with run_id (orthogonal to tier)
            run_id = (ev.metadata or {}).get("run_id")
            if run_id:
                with self._lock:
                    idx = self._run_event_counters.get(run_id, 0)
                    self._run_event_counters[run_id] = idx + 1
                re_rows.append(_to_re_values(ev, run_id, idx))

        if ce_rows:
            for row in ce_rows:
                db.execute(_CE_INSERT, row)
        if re_rows:
            for row in re_rows:
                db.execute(_RE_INSERT, row)
        if ce_rows or re_rows:
            db.commit()

    _SENTINEL = object()  # Poison pill to unblock Queue.get()

    def shutdown(self, timeout: float = 2.0) -> "asyncio.Task | None":
        """Graceful shutdown. Returns the flush task (if any) for callers to await."""
        self._closed = True

        # Deregister atexit to avoid double-shutdown
        if self._atexit_registered:
            atexit.unregister(self._atexit_flush)
            self._atexit_registered = False

        # Unblock _drain()'s Queue.get() so _flush_loop exits its while-loop naturally
        try:
            self._queue.put_nowait(self._SENTINEL)  # type: ignore[arg-type]
        except Exception:
            pass

        task = self._flush_task
        self._flush_task = None

        if task is None or task.done():
            # No background task — drain synchronously
            self._sync_drain_and_flush()
            return None

        # Background task is running — it will drain remaining events as it exits.
        # Return the task so async callers can await it.
        return task

    def _sync_drain_and_flush(self) -> None:
        """Drain queue and flush remaining events synchronously."""
        remaining: list[ConversationEvent] = []
        try:
            while not self._queue.empty():
                item = self._queue.get_nowait()
                if item is not self._SENTINEL:
                    remaining.append(item)
        except Exception:
            pass

        if remaining:
            db = self._db_factory()
            try:
                self._do_flush(db, remaining)
                self.stats["flushed"] += len(remaining)
                logger.info("Shutdown: flushed %d remaining events", len(remaining))
            except Exception:
                db.rollback()
                logger.exception("Shutdown flush failed (%d events)", len(remaining))
                self.stats["dropped"] += len(remaining)
            finally:
                db.close()

    def _atexit_flush(self) -> None:
        """Best-effort flush on process exit. Must never raise."""
        try:
            if not self._closed:
                # At atexit, the event loop is closed. Don't try to cancel tasks
                # or put sentinels — just mark closed and drain synchronously.
                self._closed = True
                self._flush_task = None  # Let GC handle the task

                remaining: list[ConversationEvent] = []
                try:
                    while not self._queue.empty():
                        item = self._queue.get_nowait()
                        if isinstance(item, ConversationEvent):
                            remaining.append(item)
                except Exception:
                    pass

                if remaining:
                    db = self._db_factory()
                    try:
                        self._do_flush(db, remaining)
                    except Exception:
                        try:
                            db.rollback()
                        except Exception:
                            pass
                    finally:
                        db.close()
        except Exception:
            pass
