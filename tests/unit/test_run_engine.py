"""Tests for RunEngine: start, resume, cancel, timeout, error handling."""

import asyncio
import pytest
from unittest.mock import MagicMock, patch, AsyncMock
from datetime import datetime, timezone

from core.agent.run import AgentRun, RunStatus, RunTrigger
from core.agent.run_engine import RunEngine, _active_runs, _run_events, _run_waiters
from core.events.models import EventType


@pytest.fixture
def mock_db():
    return MagicMock()


@pytest.fixture
def engine(mock_db):
    with patch.object(RunEngine, '__init__', lambda self, db: setattr(self, 'db', db) or setattr(self, 'event_logger', MagicMock())):
        e = RunEngine(mock_db)
        return e


@pytest.fixture(autouse=True)
def clean_state():
    """Clean global state before each test."""
    _active_runs.clear()
    _run_events.clear()
    _run_waiters.clear()
    yield
    _active_runs.clear()
    _run_events.clear()
    _run_waiters.clear()


class TestRunEngineCreate:

    def test_create_run(self, engine):
        run = engine.create_run(
            session_id="s1", user_id="u1", user_input="hello",
        )
        assert run.status == RunStatus.PENDING
        assert run.run_id in _active_runs
        assert run.run_id in _run_events
        assert run.run_id in _run_waiters

    def test_create_run_with_parent(self, engine):
        run = engine.create_run(
            session_id="s1", user_id="u1", user_input="sub",
            parent_run_id="parent_1",
        )
        assert run.parent_run_id == "parent_1"


class TestRunEngineStartRun:

    @pytest.mark.asyncio
    async def test_successful_run(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def fake_stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"text": "hello"})

        mock_loop.run_step_stream = fake_stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.start_run(run)

        assert run.status == RunStatus.COMPLETED

    @pytest.mark.asyncio
    async def test_run_failure(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def failing_stream(**kw):
            raise RuntimeError("LLM exploded")
            yield  # make it a generator

        mock_loop.run_step_stream = failing_stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.start_run(run)

        assert run.status == RunStatus.FAILED
        events = _run_events[run.run_id]
        assert any("LLM exploded" in str(e.get("data", {})) for e in events)

    @pytest.mark.asyncio
    async def test_run_timeout(self, engine):
        run = engine.create_run(
            session_id="s1", user_id="u1", user_input="hi",
            context={"run_timeout_seconds": 0.05},
        )

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def slow_stream(**kw):
            await asyncio.sleep(5)
            yield  # never reached

        mock_loop.run_step_stream = slow_stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.start_run(run)

        assert run.status == RunStatus.FAILED
        events = _run_events[run.run_id]
        assert any("timed out" in str(e.get("data", {})) for e in events)

    @pytest.mark.asyncio
    async def test_run_parks_on_wait_for(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="train model")

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def wait_stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="tool_result", data={"wait_for": "job:123"})

        mock_loop.run_step_stream = wait_stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.start_run(run)

        assert run.status == RunStatus.WAITING
        assert run.waiting_for == "job:123"


class TestRunEngineResume:

    @pytest.mark.asyncio
    async def test_resume_injects_result(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="train")
        run.status = RunStatus.WAITING
        run.waiting_for = "job:1"
        _active_runs[run.run_id] = run

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def resume_stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"text": "done"})

        mock_loop.run_step_stream = resume_stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.resume_run(run.run_id, {"accuracy": 0.95})

        assert run.status == RunStatus.COMPLETED
        assert "job:1" in run.user_input
        assert run.context["async_result"] == {"accuracy": 0.95}

    @pytest.mark.asyncio
    async def test_resume_non_waiting_run(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.COMPLETED
        _active_runs[run.run_id] = run

        # Should be a no-op
        await engine.resume_run(run.run_id, {"data": 1})
        assert run.status == RunStatus.COMPLETED  # unchanged


class TestRunEngineCancel:

    def test_cancel_running(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.RUNNING
        _active_runs[run.run_id] = run

        assert engine.cancel_run(run.run_id) is True
        assert run.status == RunStatus.CANCELLED

    def test_cancel_completed_noop(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.COMPLETED
        _active_runs[run.run_id] = run

        assert engine.cancel_run(run.run_id) is False

    def test_cancel_unknown_run(self, engine):
        assert engine.cancel_run("nonexistent") is False


class TestRunEngineResolveHandle:

    @pytest.mark.asyncio
    async def test_resolve_workflow_inner_wait(self, engine):
        with patch("core.agent.async_tools.resume_workflow", new_callable=AsyncMock, return_value=True):
            result = await engine.resolve_handle("approval:1", {"approved": True})
        assert result is True

    @pytest.mark.asyncio
    async def test_resolve_direct_handle(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.WAITING
        run.waiting_for = "job:1"
        _active_runs[run.run_id] = run

        reg = get_async_tool_registry()
        reg._handle_to_run["job:1"] = run.run_id

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"text": "ok"})

        mock_loop.run_step_stream = stream

        with patch("core.agent.async_tools.resume_workflow", new_callable=AsyncMock, return_value=False), \
             patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            result = await engine.resolve_handle("job:1", {"status": "done"})

        assert result is True

    @pytest.mark.asyncio
    async def test_resolve_unknown_handle(self, engine):
        with patch("core.agent.async_tools.resume_workflow", new_callable=AsyncMock, return_value=False):
            result = await engine.resolve_handle("unknown:x", {})
        assert result is False


def get_async_tool_registry():
    from core.agent.async_tools import get_async_tool_registry as _get
    return _get()
