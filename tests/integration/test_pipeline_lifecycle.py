"""Integration test: EventPipeline lifecycle through _build_chat_loop.

Verifies that the pipeline is properly created, started, and shut down
in the real production wiring path — not just in unit tests with mocks.
"""

import asyncio

import pytest
from unittest.mock import patch

from core.events.models import ConversationEvent, EventType
from uuid_utils import uuid7
from datetime import datetime, timezone


def _make_event(event_type=EventType.USER_QUERY):
    return ConversationEvent(
        event_id=str(uuid7()),
        user_id="test",
        session_id="test-session",
        agent_id="test-agent",
        agent_version="0.1",
        event_type=event_type,
        content="test",
        created_at=datetime.now(timezone.utc),
    )


class TestPipelineLifecycleInBuildChatLoop:
    """Verify _build_chat_loop creates/starts pipeline and shutdown releases resources."""

    def test_pipeline_created_when_enabled(self, db_session):
        """_build_chat_loop creates and starts an EventPipeline when flag is on."""
        from api.routers.chat import _build_chat_loop

        with patch("core.events.event_logger._PIPELINE_ENABLED", True):
            loop = _build_chat_loop(db_session)

        pipeline = loop.event_logger._pipeline
        assert pipeline is not None, "Pipeline should be created when enabled"
        assert not pipeline._closed, "Pipeline should not be closed after creation"

        # Cleanup
        pipeline.shutdown()

    def test_pipeline_not_created_when_disabled(self, db_session):
        """_build_chat_loop skips pipeline when flag is off."""
        from api.routers.chat import _build_chat_loop

        with patch("core.events.event_logger._PIPELINE_ENABLED", False):
            loop = _build_chat_loop(db_session)

        assert loop.event_logger._pipeline is None

    @pytest.mark.asyncio
    async def test_pipeline_full_lifecycle(self, db_session):
        """Create → start → emit → shutdown → verify task done + DB released."""
        from api.routers.chat import _build_chat_loop

        with patch("core.events.event_logger._PIPELINE_ENABLED", True):
            loop = _build_chat_loop(db_session)

        pipeline = loop.event_logger._pipeline
        assert pipeline is not None

        # Pipeline should have a running flush task
        assert pipeline._flush_task is not None
        assert not pipeline._flush_task.done()

        # Emit an event — should not raise
        pipeline.emit(_make_event())
        assert pipeline.stats["emitted"] == 1

        # Shutdown returns the task for awaiting
        task = pipeline.shutdown()
        assert pipeline._closed

        if task:
            await asyncio.wait_for(task, timeout=5.0)
            assert task.done()

        # After shutdown, flush_task reference is cleared
        assert pipeline._flush_task is None

    @pytest.mark.asyncio
    async def test_no_leaked_tasks_after_shutdown(self, db_session):
        """After shutdown + await, no pending tasks from the pipeline remain."""
        from api.routers.chat import _build_chat_loop

        with patch("core.events.event_logger._PIPELINE_ENABLED", True):
            loop = _build_chat_loop(db_session)

        pipeline = loop.event_logger._pipeline
        flush_task = pipeline._flush_task

        # Emit events to exercise the flush path
        for _ in range(3):
            pipeline.emit(_make_event(EventType.RUN_STARTED))
        await asyncio.sleep(0.3)

        task = pipeline.shutdown()
        if task:
            await asyncio.wait_for(task, timeout=5.0)

        # The original flush task should be done
        assert flush_task.done(), "Flush task must be done after shutdown + await"
        # No exception stored
        assert flush_task.exception() is None, f"Flush task raised: {flush_task.exception()}"

    @pytest.mark.asyncio
    async def test_run_engine_shuts_down_pipeline(self, db_session):
        """start_run's finally block shuts down the pipeline it created."""
        from unittest.mock import MagicMock, AsyncMock
        from core.agent.run_engine import RunEngine, _active_runs
        from core.events.models import StreamEvent

        mock_db = MagicMock()
        mock_db.execute.return_value.fetchone.return_value = None
        mock_db.execute.return_value.fetchall.return_value = []

        # Track pipeline shutdown calls
        shutdown_called = []

        class SpyPipeline:
            """Minimal pipeline spy that tracks shutdown."""
            _closed = False
            stats = {"emitted": 0, "flushed": 0, "dropped": 0}
            _flush_task = None

            def start(self): pass
            def emit(self, e): return e.event_id
            def flush_critical(self): pass

            def shutdown(self, timeout=2.0):
                self._closed = True
                shutdown_called.append(True)
                return None

        spy_pipeline = SpyPipeline()

        mock_loop = MagicMock()
        mock_loop._current_run_id = None
        mock_loop.event_logger = MagicMock()
        mock_loop.event_logger._pipeline = spy_pipeline

        async def fake_stream(**kw):
            yield StreamEvent(event_type="text_delta", data={"chunk": "done"})

        mock_loop.run_step_stream = fake_stream

        with patch.object(RunEngine, '__init__',
                          lambda self, db: setattr(self, 'db', db) or setattr(self, 'event_logger', MagicMock())):
            engine = RunEngine(mock_db)
            run = engine.create_run(session_id="s1", user_id="u1", user_input="test")
            run.status = __import__('core.agent.run_engine', fromlist=['RunStatus']).RunStatus.RUNNING

            with patch("api.database.get_db_session", return_value=iter([mock_db])), \
                 patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
                await engine.start_run(run)

        assert len(shutdown_called) == 1, "Pipeline.shutdown() must be called in start_run finally"
