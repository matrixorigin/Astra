"""Tests for RunEngine: start, resume, cancel, timeout, error handling."""

import asyncio
import pytest
from unittest.mock import MagicMock, patch, AsyncMock
from datetime import datetime, timezone

from core.agent.run import AgentRun, RunStatus, RunTrigger
from core.agent.run_engine import RunEngine, _active_runs, _run_events, _run_waiters, _run_tasks, _child_runs
from core.events.models import EventType


@pytest.fixture
def mock_db():
    db = MagicMock()
    # Default: DB queries return no results (no cancel events, no waiting runs)
    db.execute.return_value.fetchone.return_value = None
    db.execute.return_value.fetchall.return_value = []
    return db


@pytest.fixture
def engine(mock_db):
    with patch.object(RunEngine, '__init__', lambda self, db: setattr(self, 'db', db) or setattr(self, 'event_logger', MagicMock())):
        e = RunEngine(mock_db)
        # Default: claim always succeeds (single-worker behavior)
        e._try_claim_resume = MagicMock(return_value=True)
        return e


@pytest.fixture(autouse=True)
def clean_state():
    """Clean global state before each test."""
    _active_runs.clear()
    _run_events.clear()
    _run_waiters.clear()
    _run_tasks.clear()
    _child_runs.clear()
    yield
    _active_runs.clear()
    _run_events.clear()
    _run_waiters.clear()
    _run_tasks.clear()
    _child_runs.clear()


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

    def test_cancel_kills_task(self, engine):
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.RUNNING
        _active_runs[run.run_id] = run

        mock_task = MagicMock()
        mock_task.done.return_value = False
        _run_tasks[run.run_id] = mock_task

        engine.cancel_run(run.run_id)
        mock_task.cancel.assert_called_once()
        assert run.run_id not in _run_tasks

    def test_cancel_propagates_to_workflow(self, engine):
        from core.agent.async_tools import _workflow_runs, _workflow_waits

        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.WAITING
        run.waiting_for = "workflow:wf_123"
        _active_runs[run.run_id] = run

        mock_engine = MagicMock()
        mock_wf = MagicMock()
        mock_wf.name = "test_wf"
        _workflow_runs["wf_123"] = {"workflow": mock_wf, "engine": mock_engine, "wf_run": None}
        _workflow_waits["inner:handle"] = "wf_123"

        engine.cancel_run(run.run_id)

        assert run.status == RunStatus.CANCELLED
        mock_engine.cancel.assert_called_once_with("test_wf")
        assert "wf_123" not in _workflow_runs
        assert "inner:handle" not in _workflow_waits

    @pytest.mark.asyncio
    async def test_resume_cancelled_run_from_db(self, engine):
        """If run was cancelled while waiting (from another worker), resume should detect it."""
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.WAITING
        run.waiting_for = "job:1"
        _active_runs[run.run_id] = run

        # Simulate cancel event in DB
        engine.db.execute.return_value.fetchone.return_value = (1,)

        await engine.resume_run(run.run_id, {"data": "result"})
        assert run.status == RunStatus.CANCELLED

    @pytest.mark.asyncio
    async def test_resume_claim_rejected(self, engine):
        """If another worker already claimed the resume, this one should skip."""
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.WAITING
        run.waiting_for = "job:1"
        _active_runs[run.run_id] = run

        engine._try_claim_resume = MagicMock(return_value=False)

        await engine.resume_run(run.run_id, {"data": "result"})
        # Run should still be WAITING — not resumed
        assert run.status == RunStatus.WAITING


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


class TestTryClaimResume:
    """Test the real _try_claim_resume logic (not mocked)."""

    def _make_engine(self, mock_db):
        with patch.object(RunEngine, '__init__', lambda self, db: setattr(self, 'db', db) or setattr(self, 'event_logger', MagicMock())):
            return RunEngine(mock_db)

    def test_claim_succeeds_on_first_insert(self):
        db = MagicMock()
        db.execute.return_value = MagicMock()
        engine = self._make_engine(db)
        assert engine._try_claim_resume("run-1") is True
        db.commit.assert_called_once()

    def test_claim_fails_on_integrity_error(self):
        from sqlalchemy.exc import IntegrityError
        db = MagicMock()
        db.execute.side_effect = IntegrityError("dup", {}, None)
        engine = self._make_engine(db)
        assert engine._try_claim_resume("run-1") is False
        db.rollback.assert_called_once()

    def test_claim_fallback_on_unexpected_error(self):
        db = MagicMock()
        db.execute.side_effect = RuntimeError("connection lost")
        engine = self._make_engine(db)
        # Fallback: allow resume in single-worker mode
        assert engine._try_claim_resume("run-1") is True


class TestMultiAgentRuns:
    """Test child run creation, fan-out, and fan-in."""

    @pytest.mark.asyncio
    async def test_create_child_run(self, engine):
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review code")
        parent.status = RunStatus.RUNNING

        mock_loop = MagicMock()
        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"text": "reviewed"})
        mock_loop.run_step_stream = stream
        mock_loop._current_run_id = None

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            child = await engine.create_child_run(
                parent_run_id=parent.run_id,
                agent_id="security_reviewer",
                task="Review for security issues",
            )

        assert child.parent_run_id == parent.run_id
        assert child.agent_id == "security_reviewer"
        assert child.session_id == parent.session_id
        assert child.run_id in _child_runs[parent.run_id]
        assert child.run_id in _run_tasks
        # Wait for child to finish
        await _run_tasks[child.run_id]

    @pytest.mark.asyncio
    async def test_create_child_run_unknown_parent(self, engine):
        with pytest.raises(ValueError, match="not found"):
            await engine.create_child_run(
                parent_run_id="nonexistent",
                agent_id="reviewer",
                task="review",
            )

    @pytest.mark.asyncio
    async def test_fan_in_resumes_parent(self, engine):
        """When all children complete, parent should be resumed."""
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="multi-review")
        parent.status = RunStatus.WAITING
        parent.waiting_for = f"children:{parent.run_id}"

        mock_loop = MagicMock()
        mock_loop._current_run_id = None
        call_count = 0

        async def stream(**kw):
            from core.events.models import StreamEvent
            nonlocal call_count
            call_count += 1
            yield StreamEvent(event_type="text_delta", data={"text": f"result-{call_count}"})
        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            c1 = await engine.create_child_run(parent.run_id, "reviewer_a", "review A")
            c2 = await engine.create_child_run(parent.run_id, "reviewer_b", "review B")

            # Let all tasks complete (children + fan-in resume)
            await asyncio.sleep(0.1)

        assert c1.status == RunStatus.COMPLETED
        assert c2.status == RunStatus.COMPLETED
        # Parent should have been resumed (status changed from WAITING)
        assert parent.status == RunStatus.COMPLETED
        assert parent.run_id not in _child_runs

    @pytest.mark.asyncio
    async def test_fan_in_waits_for_all(self, engine):
        """Fan-in should NOT resume parent until ALL children are done."""
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="multi")
        parent.status = RunStatus.WAITING
        parent.waiting_for = f"children:{parent.run_id}"

        # Create two children manually (don't start them)
        c1 = engine.create_run(session_id="s1", user_id="u1", user_input="task1",
                               agent_id="a1", parent_run_id=parent.run_id)
        c2 = engine.create_run(session_id="s1", user_id="u1", user_input="task2",
                               agent_id="a2", parent_run_id=parent.run_id)
        _child_runs.setdefault(parent.run_id, set()).update({c1.run_id, c2.run_id})

        # Only c1 completes
        c1.status = RunStatus.COMPLETED
        c1.completed_at = datetime.now(timezone.utc)

        await engine._check_fan_in(parent.run_id)
        # Parent still waiting — c2 not done
        assert parent.status == RunStatus.WAITING
        assert parent.run_id in _child_runs


def get_async_tool_registry():
    from core.agent.async_tools import get_async_tool_registry as _get
    return _get()
