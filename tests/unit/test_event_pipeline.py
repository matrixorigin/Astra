"""Tests for EventPipeline — async event ingestion with background flush."""

import asyncio
import json
from datetime import datetime, timezone
from unittest.mock import MagicMock, call, patch

import pytest
from uuid_utils import uuid7

from core.events.models import ConversationEvent, EventType
from core.events.pipeline import (
    CRITICAL_TYPES,
    DURABLE_TYPES,
    EventPipeline,
    _to_ce_values,
    _to_re_values,
)


def _make_event(
    event_type: EventType = EventType.USER_QUERY,
    content: str = "test",
    metadata: dict | None = None,
) -> ConversationEvent:
    return ConversationEvent(
        event_id=str(uuid7()),
        user_id="u1",
        session_id="s1",
        agent_id="dev-agent",
        agent_version="0.1.0",
        event_type=event_type,
        content=content,
        causal_chain_id=str(uuid7()),
        metadata=metadata,
    )


class FakeSession:
    """Minimal mock for SQLAlchemy Session."""

    def __init__(self):
        self.executed = []
        self.committed = 0
        self.rolled_back = 0
        self._closed = False

    def execute(self, stmt, params=None):
        self.executed.append((stmt, params))

    def commit(self):
        self.committed += 1

    def rollback(self):
        self.rolled_back += 1

    def close(self):
        self._closed = True


@pytest.fixture
def fake_db():
    sessions = []

    def factory():
        s = FakeSession()
        sessions.append(s)
        return s

    return factory, sessions


# --- Classification tests ---


class TestEventClassification:
    def test_critical_types(self):
        assert EventType.USER_QUERY in CRITICAL_TYPES
        assert EventType.LLM_RESPONSE in CRITICAL_TYPES
        assert len(CRITICAL_TYPES) == 2

    def test_durable_types(self):
        assert EventType.RUN_STARTED in DURABLE_TYPES
        assert EventType.RUN_COMPLETED in DURABLE_TYPES
        assert EventType.RUN_FAILED in DURABLE_TYPES
        assert EventType.PLAN_CREATED in DURABLE_TYPES

    def test_ephemeral_not_in_critical_or_durable(self):
        assert EventType.STREAM_TEXT_DELTA not in CRITICAL_TYPES
        assert EventType.STREAM_TEXT_DELTA not in DURABLE_TYPES


# --- Emit tests ---


class TestEmit:
    def test_emit_returns_event_id(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        ev = _make_event()
        result = pipeline.emit(ev)
        assert result == ev.event_id
        assert pipeline.stats["emitted"] == 1

    def test_emit_enqueues(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        ev = _make_event()
        pipeline.emit(ev)
        assert pipeline._queue.qsize() == 1

    def test_emit_after_close_raises(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        pipeline._closed = True
        with pytest.raises(RuntimeError, match="closed"):
            pipeline.emit(_make_event())


# --- flush_critical tests ---


class TestFlushCritical:
    def test_flushes_critical_events(self, fake_db):
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)

        # Enqueue mix of critical and non-critical
        crit = _make_event(EventType.USER_QUERY, "query")
        eph = _make_event(EventType.STREAM_TEXT_DELTA, "chunk")
        pipeline.emit(crit)
        pipeline.emit(eph)

        pipeline.flush_critical()

        # Critical flushed
        assert pipeline.stats["flushed"] == 1
        # Ephemeral re-enqueued
        assert pipeline._queue.qsize() == 1

    def test_flush_critical_noop_when_empty(self, fake_db):
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.flush_critical()
        assert len(sessions) == 0  # No DB session created

    def test_flush_critical_commits(self, fake_db):
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.emit(_make_event(EventType.LLM_RESPONSE, "resp"))
        pipeline.flush_critical()
        assert sessions[0].committed == 1
        assert sessions[0]._closed


# --- _do_flush routing tests ---


class TestDoFlush:
    def test_critical_routes_to_agent_events(self, fake_db):
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        db = factory()
        ev = _make_event(EventType.USER_QUERY)
        pipeline._do_flush(db, [ev])
        # Should have executed INSERT for agent_events
        assert db.committed == 1
        assert len(db.executed) == 1  # 1 CE insert, 0 RE

    def test_durable_routes_to_agent_events(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        db = factory()
        ev = _make_event(EventType.RUN_COMPLETED)
        pipeline._do_flush(db, [ev])
        assert db.committed == 1
        assert len(db.executed) == 1

    def test_ephemeral_without_run_id_skipped(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        db = factory()
        ev = _make_event(EventType.STREAM_TEXT_DELTA)
        pipeline._do_flush(db, [ev])
        # No CE, no RE (no run_id)
        assert db.committed == 0
        assert len(db.executed) == 0

    def test_ephemeral_with_run_id_routes_to_agent_run_events(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        db = factory()
        ev = _make_event(EventType.STREAM_TEXT_DELTA, metadata={"run_id": "r1"})
        pipeline._do_flush(db, [ev])
        # Only RE insert (ephemeral → no CE)
        assert db.committed == 1
        assert len(db.executed) == 1

    def test_critical_with_run_id_dual_writes(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        db = factory()
        ev = _make_event(EventType.USER_QUERY, metadata={"run_id": "r1"})
        pipeline._do_flush(db, [ev])
        # CE + RE = 2 inserts
        assert len(db.executed) == 2
        assert db.committed == 1

    def test_run_event_idx_increments(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        db = factory()
        e1 = _make_event(EventType.STREAM_TEXT_DELTA, metadata={"run_id": "r1"})
        e2 = _make_event(EventType.STREAM_TEXT_DELTA, metadata={"run_id": "r1"})
        pipeline._do_flush(db, [e1, e2])
        # Two RE inserts with idx 0 and 1
        assert len(db.executed) == 2
        params0 = db.executed[0][1]
        params1 = db.executed[1][1]
        assert params0["idx"] == 0
        assert params1["idx"] == 1


# --- Background flush loop tests ---


class TestFlushLoop:
    @pytest.mark.asyncio
    async def test_flush_loop_drains_queue(self, fake_db):
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.FLUSH_INTERVAL_S = 0.05  # Speed up for test

        # Emit events
        for i in range(5):
            pipeline.emit(_make_event(EventType.RUN_STARTED, f"ev{i}"))

        # Run flush loop briefly
        pipeline.start()
        await asyncio.sleep(0.2)
        task = pipeline.shutdown()
        if task:
            try:
                await asyncio.wait_for(task, timeout=1.0)
            except (asyncio.CancelledError, asyncio.TimeoutError):
                pass

        assert pipeline.stats["flushed"] >= 5
        assert pipeline._queue.qsize() == 0

    @pytest.mark.asyncio
    async def test_flush_loop_handles_empty_queue(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)
        pipeline.FLUSH_INTERVAL_S = 0.05

        pipeline.start()
        await asyncio.sleep(0.15)
        task = pipeline.shutdown()
        if task:
            try:
                await asyncio.wait_for(task, timeout=1.0)
            except (asyncio.CancelledError, asyncio.TimeoutError):
                pass

        # No events emitted, no flushes
        assert pipeline.stats["flushed"] == 0


# --- Shutdown tests ---


class TestShutdown:
    def test_shutdown_drains_remaining(self, fake_db):
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)

        for i in range(10):
            pipeline.emit(_make_event(EventType.RUN_STARTED, f"ev{i}"))

        pipeline.shutdown()
        assert pipeline._closed
        assert pipeline.stats["flushed"] == 10
        assert pipeline._queue.qsize() == 0

    def test_shutdown_noop_when_empty(self, fake_db):
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.shutdown()
        assert pipeline._closed
        assert len(sessions) == 0


# --- Conversion helper tests ---


class TestConversionHelpers:
    def test_to_ce_values_basic(self):
        ev = _make_event(EventType.USER_QUERY, "hello")
        row = _to_ce_values(ev)
        assert row["event_id"] == ev.event_id
        assert row["event_type"] == "user_query"
        assert row["content"] == "hello"
        assert row["run_id"] is None

    def test_to_ce_values_with_run_id(self):
        ev = _make_event(EventType.USER_QUERY, metadata={"run_id": "r1"})
        row = _to_ce_values(ev)
        assert row["run_id"] == "r1"

    def test_to_re_values(self):
        ev = _make_event(EventType.STREAM_TEXT_DELTA, "chunk")
        row = _to_re_values(ev, "r1", 5)
        assert row["run_id"] == "r1"
        assert row["idx"] == 5
        assert row["event_type"] == "stream_text_delta"


# --- Backpressure tests ---


class TestBackpressure:
    def test_drops_ephemeral_at_threshold(self, fake_db):
        factory, _ = fake_db
        pipeline = EventPipeline(factory)

        # Simulate full queue by putting sentinel items
        for _ in range(100_000):
            pipeline._queue.put_nowait(
                _make_event(EventType.STREAM_TEXT_DELTA, "x")
            )
        pipeline.stats["emitted"] = 100_000

        # Ephemeral should be dropped
        eph = _make_event(EventType.STREAM_TEXT_DELTA, "dropped")
        pipeline.emit(eph)
        assert pipeline.stats["dropped"] == 1

        # Critical should NOT be dropped
        crit = _make_event(EventType.USER_QUERY, "kept")
        pipeline.emit(crit)
        assert pipeline.stats["dropped"] == 1  # Still 1
        assert pipeline._queue.qsize() == 100_001  # Critical was enqueued


# --- Batch size tests ---


class TestBatchEmitAndFlush:
    def test_emit_1000_events_all_flushed(self, fake_db):
        """Validation from design: emit 1000 events → all flushed."""
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)

        for i in range(1000):
            pipeline.emit(_make_event(EventType.RUN_STARTED, f"ev{i}"))

        pipeline.shutdown()
        assert pipeline.stats["flushed"] == 1000
        assert pipeline.stats["dropped"] == 0


# --- Error handling / transaction safety tests ---


class TestErrorHandling:
    def test_flush_critical_rollback_on_failure(self, fake_db):
        """flush_critical must rollback on DB error, not leave dirty session."""
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.emit(_make_event(EventType.USER_QUERY, "q"))

        # Make execute raise
        def bad_factory():
            s = FakeSession()
            s.execute = lambda *a, **kw: (_ for _ in ()).throw(RuntimeError("DB down"))
            sessions.append(s)
            return s

        pipeline._db_factory = bad_factory
        pipeline.flush_critical()

        assert pipeline.stats["dropped"] == 1
        assert sessions[-1].rolled_back == 1
        assert sessions[-1]._closed

    def test_shutdown_rollback_on_failure(self, fake_db):
        """shutdown must rollback on DB error."""
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.emit(_make_event(EventType.RUN_STARTED, "ev"))

        def bad_factory():
            s = FakeSession()
            s.execute = lambda *a, **kw: (_ for _ in ()).throw(RuntimeError("DB down"))
            sessions.append(s)
            return s

        pipeline._db_factory = bad_factory
        pipeline.shutdown()

        assert pipeline.stats["dropped"] == 1
        assert sessions[-1].rolled_back == 1
        assert sessions[-1]._closed

    @pytest.mark.asyncio
    async def test_flush_loop_retries_once_on_failure(self, fake_db):
        """Background flush retries once before dropping."""
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.FLUSH_INTERVAL_S = 0.05

        call_count = 0
        original_do_flush = pipeline._do_flush

        def flaky_flush(db, events):
            nonlocal call_count
            call_count += 1
            if call_count == 1:
                raise RuntimeError("transient error")
            return original_do_flush(db, events)

        pipeline._do_flush = flaky_flush
        pipeline.emit(_make_event(EventType.RUN_STARTED, "ev"))

        pipeline.start()
        await asyncio.sleep(0.2)
        task = pipeline.shutdown()
        if task:
            try:
                await asyncio.wait_for(task, timeout=1.0)
            except (asyncio.CancelledError, asyncio.TimeoutError):
                pass

        # Retry succeeded on second attempt
        assert call_count == 2
        assert pipeline.stats["flushed"] == 1
        assert pipeline.stats["dropped"] == 0

    @pytest.mark.asyncio
    async def test_flush_loop_drops_after_retry_fails(self, fake_db):
        """Background flush drops events if retry also fails."""
        factory, sessions = fake_db
        pipeline = EventPipeline(factory)
        pipeline.FLUSH_INTERVAL_S = 0.05

        def always_fail(db, events):
            raise RuntimeError("persistent error")

        pipeline._do_flush = always_fail
        pipeline.emit(_make_event(EventType.RUN_STARTED, "ev"))

        pipeline.start()
        await asyncio.sleep(0.2)
        task = pipeline.shutdown()
        if task:
            try:
                await asyncio.wait_for(task, timeout=1.0)
            except (asyncio.CancelledError, asyncio.TimeoutError):
                pass

        assert pipeline.stats["dropped"] == 1
        assert pipeline.stats["flushed"] == 0

    def test_shutdown_survives_closed_event_loop(self, fake_db):
        """shutdown() must not crash even with a stale flush_task reference."""
        factory, _ = fake_db
        pipeline = EventPipeline(factory)

        # Simulate a flush_task that's still "running"
        from unittest.mock import MagicMock
        mock_task = MagicMock()
        mock_task.done.return_value = False
        pipeline._flush_task = mock_task

        # Should not raise — sentinel + _closed flag handle shutdown
        pipeline.shutdown()
        assert pipeline._closed
        assert pipeline._flush_task is None

    def test_atexit_flush_survives_any_error(self, fake_db):
        """_atexit_flush must never propagate exceptions."""
        factory, _ = fake_db
        pipeline = EventPipeline(factory)

        # Make shutdown raise
        def bad_shutdown(timeout=2.0):
            raise RuntimeError("everything is broken")

        pipeline.shutdown = bad_shutdown
        # Should not raise
        pipeline._atexit_flush()
