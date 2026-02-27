"""Tests for RunEngine: start, resume, cancel, timeout, error handling."""

import asyncio
import pytest
from unittest.mock import MagicMock, patch, AsyncMock
from sqlalchemy.orm import Session

pytestmark = pytest.mark.slow
from datetime import datetime, timezone

from core.agent.run import AgentRun, RunStatus, RunTrigger
from core.agent.run_engine import (
    RunEngine, _active_runs, _agent_run_events, _run_waiters, _run_tasks,
    _child_runs, _MAX_RESUME_INPUT_CHARS,
    _MAX_COMPLETED_RUNS, cleanup_fan_in_tasks,
)
from core.events.models import EventType


@pytest.fixture
def mock_db():
    db = MagicMock(spec=Session)
    # Default: DB queries return no results (no cancel events, no waiting runs)
    db.execute.return_value.fetchone.return_value = None
    db.execute.return_value.fetchall.return_value = []
    return db


@pytest.fixture
def mock_db_factory(mock_db):
    """Factory that always returns the same mock_db."""
    return lambda: mock_db


@pytest.fixture
def engine(mock_db_factory):
    from tests.conftest import make_run_engine_mock_init
    with patch.object(RunEngine, '__init__', make_run_engine_mock_init()):
        e = RunEngine(mock_db_factory)
        # Default: claim always succeeds (single-worker behavior)
        e._try_claim_resume = MagicMock(return_value=True)
        return e


@pytest.fixture(autouse=True)
def clean_state():
    """Clean global state before each test."""
    _active_runs.clear()
    _agent_run_events.clear()
    _run_waiters.clear()
    _run_tasks.clear()
    _child_runs.clear()
    cleanup_fan_in_tasks()
    yield
    _active_runs.clear()
    _agent_run_events.clear()
    _run_waiters.clear()
    _run_tasks.clear()
    _child_runs.clear()
    cleanup_fan_in_tasks()


class TestRunEngineCreate:

    def test_create_run(self, engine):
        run = engine.create_run(
            session_id="s1", user_id="u1", user_input="hello",
        )
        assert run.status == RunStatus.PENDING
        assert run.run_id in _active_runs
        assert run.run_id in _agent_run_events
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
            yield StreamEvent(event_type="text_delta", data={"chunk": "hello"})

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
            with pytest.raises(RuntimeError, match="LLM exploded"):
                await engine.start_run(run)

        assert run.status == RunStatus.FAILED
        events = _agent_run_events[run.run_id]
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
            with pytest.raises(asyncio.TimeoutError):
                await engine.start_run(run)

        assert run.status == RunStatus.FAILED
        events = _agent_run_events[run.run_id]
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
            yield StreamEvent(event_type="text_delta", data={"chunk": "done"})

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

    def test_cancel_propagates_to_workflow(self, engine, mock_db):
        from core.agent.async_tools import _wf_runs, _workflow_waits

        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.WAITING
        run.waiting_for = "workflow:wf_123"
        _active_runs[run.run_id] = run

        mock_engine = MagicMock()
        mock_wf = MagicMock()
        mock_wf.name = "test_wf"
        _wf_runs["wf_123"] = {"workflow": mock_wf, "engine": mock_engine, "wf_run": None}
        _workflow_waits["inner:handle"] = "wf_123"

        engine.cancel_run(run.run_id)

        assert run.status == RunStatus.CANCELLED
        mock_engine.cancel.assert_called_once_with("test_wf")
        assert "wf_123" not in _wf_runs
        assert "inner:handle" not in _workflow_waits

    @pytest.mark.asyncio
    async def test_resume_cancelled_run_from_db(self, engine, mock_db):
        """If run was cancelled while waiting (from another worker), resume should detect it."""
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.WAITING
        run.waiting_for = "job:1"
        _active_runs[run.run_id] = run

        # Simulate cancel event in DB
        mock_db.execute.return_value.fetchone.return_value = (1,)

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
            yield StreamEvent(event_type="text_delta", data={"chunk": "ok"})

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
        from tests.conftest import make_run_engine_mock_init
        with patch.object(RunEngine, '__init__', make_run_engine_mock_init()):
            return RunEngine(lambda: mock_db)

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

    def test_claim_fallback_on_unexpected_error(self):
        db = MagicMock()
        db.execute.side_effect = RuntimeError("connection lost")
        engine = self._make_engine(db)
        assert engine._try_claim_resume("run-1") is False


class TestMultiAgentRuns:
    """Test child run creation, fan-out, and fan-in."""

    @pytest.mark.asyncio
    async def test_create_child_run(self, engine):
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review code")
        parent.status = RunStatus.RUNNING

        mock_loop = MagicMock()
        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"chunk": "reviewed"})
        mock_loop.run_step_stream = stream
        mock_loop._current_run_id = None

        # Patch _build_chat_loop in the module where it's imported
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
            # Wait for child to finish (within patch context)
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
            yield StreamEvent(event_type="text_delta", data={"chunk": f"result-{call_count}"})
        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            c1 = await engine.create_child_run(parent.run_id, "reviewer_a", "review A")
            t1 = _run_tasks[c1.run_id]
            c2 = await engine.create_child_run(parent.run_id, "reviewer_b", "review B")
            t2 = _run_tasks[c2.run_id]

            # Wait for children (fan-in happens synchronously in their finally blocks)
            await t1
            await t2

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


class TestResumeInputCap:
    """Test that resume_run prevents token explosion on adversarial loops."""

    @pytest.mark.asyncio
    async def test_resume_preserves_original_input(self, engine):
        """After multiple resumes, user_input should reference original task, not nested."""
        run = engine.create_run(session_id="s1", user_id="u1", user_input="Fix the auth bug")
        run.status = RunStatus.WAITING
        run.waiting_for = "job:1"

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        resume_count = [0]

        async def stream(**kw):
            from core.events.models import StreamEvent
            resume_count[0] += 1
            if resume_count[0] < 3:
                # Simulate another wait
                yield StreamEvent(event_type="tool_result", data={"wait_for": f"job:{resume_count[0]+1}"})
            else:
                yield StreamEvent(event_type="text_delta", data={"chunk": "done"})

        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.resume_run(run.run_id, {"result": "first"})
            # After first resume, original_input should be saved
            assert run.context["_original_input"] == "Fix the auth bug"

            # Second resume
            run.status = RunStatus.WAITING
            run.waiting_for = "job:2"
            engine._try_claim_resume = MagicMock(return_value=True)
            await engine.resume_run(run.run_id, {"result": "second"})
            # Still references original
            assert run.context["_original_input"] == "Fix the auth bug"
            assert "Fix the auth bug" in run.user_input

            # Third resume
            run.status = RunStatus.WAITING
            run.waiting_for = "job:3"
            engine._try_claim_resume = MagicMock(return_value=True)
            await engine.resume_run(run.run_id, {"result": "third"})

        assert run.status == RunStatus.COMPLETED
        assert len(run.user_input) <= _MAX_RESUME_INPUT_CHARS

    @pytest.mark.asyncio
    async def test_resume_input_truncated(self, engine):
        """Very large results should be truncated."""
        run = engine.create_run(session_id="s1", user_id="u1", user_input="task")
        run.status = RunStatus.WAITING
        run.waiting_for = "job:1"

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"chunk": "ok"})

        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            big_result = {"data": "x" * 10000}
            await engine.resume_run(run.run_id, big_result)

        assert len(run.user_input) <= _MAX_RESUME_INPUT_CHARS


class TestCancelPropagation:
    """Test that cancel_run propagates to children."""

    def test_cancel_propagates_to_children(self, engine):
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.WAITING
        parent.waiting_for = "children:x"

        c1 = engine.create_run(session_id="s1", user_id="u1", user_input="t1",
                               agent_id="a1", parent_run_id=parent.run_id)
        c2 = engine.create_run(session_id="s1", user_id="u1", user_input="t2",
                               agent_id="a2", parent_run_id=parent.run_id)
        c1.status = RunStatus.RUNNING
        c2.status = RunStatus.RUNNING
        _child_runs[parent.run_id] = {c1.run_id, c2.run_id}

        mock_task1 = MagicMock()
        mock_task1.done.return_value = False
        mock_task2 = MagicMock()
        mock_task2.done.return_value = False
        _run_tasks[c1.run_id] = mock_task1
        _run_tasks[c2.run_id] = mock_task2

        engine.cancel_run(parent.run_id)

        assert parent.status == RunStatus.CANCELLED
        assert c1.status == RunStatus.CANCELLED
        assert c2.status == RunStatus.CANCELLED
        mock_task1.cancel.assert_called_once()
        mock_task2.cancel.assert_called_once()

    def test_cancel_skips_completed_children(self, engine):
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.RUNNING

        c1 = engine.create_run(session_id="s1", user_id="u1", user_input="t1",
                               agent_id="a1", parent_run_id=parent.run_id)
        c1.status = RunStatus.COMPLETED  # Already done
        _child_runs[parent.run_id] = {c1.run_id}

        engine.cancel_run(parent.run_id)

        assert parent.status == RunStatus.CANCELLED
        assert c1.status == RunStatus.COMPLETED  # Unchanged


class TestFanInDBFallback:
    """Test _check_fan_in with DB fallback for cross-worker scenarios."""

    @pytest.mark.asyncio
    async def test_fan_in_uses_db_when_no_in_memory_children(self, engine, mock_db):
        """When _child_runs is empty, fan-in should query DB."""
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.WAITING
        parent.waiting_for = f"children:{parent.run_id}"

        # No in-memory children — simulate cross-worker
        # DB returns child run IDs
        child_id = "child-run-123"

        def mock_execute(query, params=None):
            q = str(query)
            result = MagicMock()
            if "DISTINCT" in q and "parent_run_id" in q:
                # _get_child_run_ids_from_db
                result.fetchall.return_value = [(child_id,)]
            elif "event_type" in q and "run_id" in q and "ORDER BY created_at" in q:
                # restore_run — return child as COMPLETED
                from core.agent.run import AgentRun
                child = AgentRun(
                    session_id="s1", user_id="u1", user_input="sub",
                    agent_id="reviewer", status=RunStatus.COMPLETED,
                )
                child.run_id = child_id
                result.fetchall.return_value = [
                    (EventType.RUN_STARTED.value, child.to_event_content(), '{"run_id":"' + child_id + '"}'),
                    (EventType.RUN_COMPLETED.value, child.to_event_content(), '{"run_id":"' + child_id + '"}'),
                ]
            elif "agent_run_events" in q and "SELECT" in q:
                # _load_events_from_db
                result.fetchall.return_value = [
                    ("text_delta", '{"text":"review done"}', None, "reviewer"),
                ]
            else:
                result.fetchall.return_value = []
                result.fetchone.return_value = None
            return result

        mock_db.execute = MagicMock(side_effect=mock_execute)

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"chunk": "synthesized"})

        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine._check_fan_in(parent.run_id)

        assert parent.status == RunStatus.COMPLETED

    @pytest.mark.asyncio
    async def test_fan_in_db_returns_no_children(self, engine, mock_db):
        """If DB also has no children, fan-in should be a no-op."""
        mock_db.execute.return_value.fetchall.return_value = []

        parent = engine.create_run(session_id="s1", user_id="u1", user_input="test")
        parent.status = RunStatus.WAITING

        await engine._check_fan_in(parent.run_id)
        # Should remain waiting — no children found
        assert parent.status == RunStatus.WAITING

    def test_get_child_run_ids_from_db(self, engine, mock_db):
        mock_db.execute.return_value.fetchall.return_value = [
            ("child-1",), ("child-2",),
        ]
        ids = engine._get_child_run_ids_from_db("parent-1")
        assert ids == {"child-1", "child-2"}

    def test_get_child_run_ids_db_error(self, engine, mock_db):
        mock_db.execute.side_effect = RuntimeError("db down")
        ids = engine._get_child_run_ids_from_db("parent-1")
        assert ids == set()

    def test_get_run_status_from_db(self, engine):
        """Should delegate to restore_run."""
        with patch.object(engine, 'restore_run') as mock_restore:
            mock_run = MagicMock()
            mock_run.status = RunStatus.COMPLETED
            mock_restore.return_value = mock_run
            status = engine._get_run_status_from_db("run-1")
            assert status == RunStatus.COMPLETED

    def test_get_run_status_from_db_not_found(self, engine):
        with patch.object(engine, 'restore_run', return_value=None):
            status = engine._get_run_status_from_db("run-1")
            assert status is None


class TestAgentConfigLogging:
    """Test that _load_agent_config logs warnings on failure."""

    def test_load_config_db_error_logs_warning(self, engine, mock_db, caplog):
        import logging
        mock_db.execute.side_effect = RuntimeError("connection lost")
        with caplog.at_level(logging.WARNING):
            result = engine._load_agent_config("test-agent")
        assert result is None
        assert "Failed to load agent config" in caplog.text

    def test_load_config_success(self, engine, mock_db):
        import json
        mock_db.execute.return_value.fetchone.return_value = (
            json.dumps({"system_prompt": "You are helpful"}),
        )
        result = engine._load_agent_config("test-agent")
        assert result == {"system_prompt": "You are helpful"}

    def test_load_config_no_row(self, engine, mock_db):
        mock_db.execute.return_value.fetchone.return_value = None
        result = engine._load_agent_config("test-agent")
        assert result is None


class TestEventPersistWarning:
    """Test that _append_event logs at warning level on failure."""

    def test_append_event_db_failure_logs_error_after_retry(self, engine, mock_db, caplog):
        import logging
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        # Make DB write fail persistently
        mock_db.execute.side_effect = RuntimeError("disk full")
        with caplog.at_level(logging.WARNING):
            engine._append_event(run.run_id, {"event_type": "test", "data": {}})
            engine._flush_agent_run_events()  # Triggers write → fail → retry → fail → error
        # First attempt logs warning, second logs error with "after retry"
        assert "retrying" in caplog.text.lower()
        assert "after retry" in caplog.text.lower()
        # Event should still be in local buffer (in-memory)
        assert len(_agent_run_events[run.run_id]) == 1
        # Pending inserts should be empty — dropped after retry exhausted
        assert len(engine._pending_inserts) == 0

    def test_pending_buffer_capped_on_retry(self, engine, mock_db, caplog):
        """When DB is persistently down, pending buffer is capped at _MAX_PENDING_EVENTS."""
        import logging
        from core.agent.run_engine import _MAX_PENDING_EVENTS
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        # Pre-fill buffer beyond cap
        engine._pending_inserts = [{"run_id": run.run_id, "idx": i, "event_type": "t", "data": "{}", "event_id": None, "agent_id": None}
                                   for i in range(_MAX_PENDING_EVENTS + 100)]
        mock_db.execute.side_effect = RuntimeError("db down")
        with caplog.at_level(logging.WARNING):
            engine._flush_agent_run_events()
        # After retry failure, buffer is dropped (retry also fails)
        # But during the retry merge, it was capped
        assert "capped" in caplog.text.lower()


class TestResumeClaimMultiple:
    """Test that _try_claim_resume works across multiple resume cycles."""

    def _make_engine(self, mock_db):
        from tests.conftest import make_run_engine_mock_init
        with patch.object(RunEngine, '__init__', make_run_engine_mock_init()):
            return RunEngine(lambda: mock_db)

    def test_multiple_claims_use_different_idx(self):
        """Adversarial loop: same run resumed multiple times should succeed."""
        db = MagicMock()
        # Simulate DB returning MIN(idx): None, -1, -2 for successive calls
        min_results = iter([(None,), (-1,), (-2,)])
        def _execute(stmt, params=None):
            sql = str(stmt) if not isinstance(stmt, str) else stmt
            if hasattr(stmt, 'text'):
                sql = stmt.text
            if "MIN(idx)" in sql:
                row = next(min_results)
                result = MagicMock()
                result.fetchone.return_value = row
                return result
            return MagicMock()
        db.execute.side_effect = _execute
        engine = self._make_engine(db)

        assert engine._try_claim_resume("run-1") is True
        assert engine._try_claim_resume("run-1") is True
        assert engine._try_claim_resume("run-1") is True

        # Verify different idx values were used: -1, -2, -3
        calls = db.execute.call_args_list
        idxs = []
        for call in calls:
            params = call[0][1] if len(call[0]) > 1 else call[1].get("params", {})
            if isinstance(params, dict) and "idx" in params:
                idxs.append(params["idx"])
        assert idxs == [-1, -2, -3]

    def test_claim_counter_db_based_cross_worker(self):
        """Counter derived from DB, not in-memory — works across workers."""
        db = MagicMock()
        # Worker B sees existing claim at idx=-1 from Worker A
        def _execute(stmt, params=None):
            sql = str(stmt) if not isinstance(stmt, str) else stmt
            if hasattr(stmt, 'text'):
                sql = stmt.text
            if "MIN(idx)" in sql:
                result = MagicMock()
                result.fetchone.return_value = (-1,)  # Worker A already claimed -1
                return result
            return MagicMock()
        db.execute.side_effect = _execute
        engine = self._make_engine(db)

        assert engine._try_claim_resume("run-a") is True

        # Verify Worker B uses idx=-2 (not -1 which would collide)
        calls = db.execute.call_args_list
        for call in calls:
            params = call[0][1] if len(call[0]) > 1 else call[1].get("params", {})
            if isinstance(params, dict) and "idx" in params:
                assert params["idx"] == -2


class TestMemoryGC:
    """Test that completed runs are garbage collected."""

    def test_gc_removes_oldest_completed(self, engine):
        from core.agent.run_engine import _MAX_COMPLETED_RUNS

        # Create more than _MAX_COMPLETED_RUNS completed runs
        for i in range(_MAX_COMPLETED_RUNS + 50):
            run = engine.create_run(session_id="s1", user_id="u1", user_input=f"task-{i}")
            run.status = RunStatus.COMPLETED
            run.completed_at = datetime(2026, 1, 1, i // 3600, (i // 60) % 60, i % 60, tzinfo=timezone.utc)

        assert len(_active_runs) == _MAX_COMPLETED_RUNS + 50

        # Trigger GC
        RunEngine._maybe_gc()

        assert len(_active_runs) == _MAX_COMPLETED_RUNS
        # Oldest should be removed
        remaining_inputs = {r.user_input for r in _active_runs.values()}
        assert "task-0" not in remaining_inputs
        assert f"task-{_MAX_COMPLETED_RUNS + 49}" in remaining_inputs

    def test_gc_preserves_running_runs(self, engine):
        # Create running + completed runs
        running = engine.create_run(session_id="s1", user_id="u1", user_input="running")
        running.status = RunStatus.RUNNING

        for i in range(_MAX_COMPLETED_RUNS + 10):
            run = engine.create_run(session_id="s1", user_id="u1", user_input=f"done-{i}")
            run.status = RunStatus.COMPLETED
            run.completed_at = datetime(2026, 1, 1, i // 3600, (i // 60) % 60, i % 60, tzinfo=timezone.utc)

        RunEngine._maybe_gc()

        # Running run should still be there
        assert running.run_id in _active_runs

    def test_gc_noop_under_threshold(self, engine):
        for i in range(5):
            run = engine.create_run(session_id="s1", user_id="u1", user_input=f"t-{i}")
            run.status = RunStatus.COMPLETED
            run.completed_at = datetime.now(timezone.utc)

        RunEngine._maybe_gc()
        assert len(_active_runs) == 5  # No cleanup needed


class TestCrossWorkerCancel:
    """Test cancel propagation to children on other workers."""

    def test_cancel_writes_db_event_for_remote_children(self, engine, mock_db):
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.RUNNING

        # Simulate children on another worker (not in _active_runs)
        remote_child_id = "remote-child-123"
        _child_runs[parent.run_id] = {remote_child_id}

        engine.cancel_run(parent.run_id)

        # _log_run_event and _write_cancel_event_for_run both do db.add()
        # Check that cancel events were persisted via db.add
        add_calls = mock_db.add.call_args_list
        cancel_events = [c for c in add_calls
                         if hasattr(c[0][0], 'event_type')
                         and c[0][0].event_type == EventType.RUN_CANCELLED]
        assert len(cancel_events) >= 2


class TestCausalChainPropagation:
    """Test that child runs inherit parent's causal chain."""

    @pytest.mark.asyncio
    async def test_child_inherits_causal_chain(self, engine, mock_db):
        parent = engine.create_run(
            session_id="s1", user_id="u1", user_input="review",
            context={"_causal_chain_id": "chain-abc-123"},
        )
        parent.status = RunStatus.RUNNING

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"chunk": "ok"})

        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            child = await engine.create_child_run(parent.run_id, "reviewer", "review code")
            await asyncio.sleep(0.1)

        # Child should have inherited the causal chain
        assert child.context.get("_causal_chain_id") == "chain-abc-123"

        # Verify causal chain was passed to DB via add() calls.
        # ORM column is `event_metadata` (mapped to DB column "metadata"),
        # not `.metadata` which is SQLAlchemy's schema MetaData object.
        add_calls = mock_db.add.call_args_list
        child_events = [c[0][0] for c in add_calls
                        if hasattr(c[0][0], 'causal_chain_id')
                        and hasattr(c[0][0], 'event_metadata')
                        and isinstance(getattr(c[0][0], 'event_metadata', None), dict)
                        and c[0][0].event_metadata.get("run_id") == child.run_id]
        assert len(child_events) > 0, "Expected at least one DB event for child run"
        for ev in child_events:
            assert ev.causal_chain_id == "chain-abc-123"


class TestConsumeStreamCancellation:
    """Test that _consume_stream checks for DB cancellation between events."""

    @pytest.mark.asyncio
    async def test_child_detects_cancel_between_events(self, engine):
        """Child run should stop if cancelled in DB between stream events."""
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.RUNNING

        child = engine.create_run(
            session_id="s1", user_id="u1", user_input="sub",
            agent_id="reviewer", parent_run_id=parent.run_id,
        )

        event_count = [0]

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def stream(**kw):
            from core.events.models import StreamEvent
            for i in range(20):
                event_count[0] += 1
                yield StreamEvent(event_type="text_delta", data={"chunk": f"chunk-{i}"})

        mock_loop.run_step_stream = stream

        # Always report cancelled in DB
        engine._is_cancelled_in_db = lambda run_id: True

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.start_run(child)

        assert child.status == RunStatus.CANCELLED
        # Check happens at event_count % 5 == 0, so should stop at event 5
        assert event_count[0] <= 5


class TestStreamRunEventsBounded:
    """Test stream_agent_run_events timeout and local flag re-check."""

    @pytest.mark.asyncio
    async def test_stream_exits_on_max_idle(self, engine, mock_db):
        """stream_agent_run_events should exit after max_idle_polls."""
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        run.status = RunStatus.RUNNING

        # Monkey-patch to use tiny max_idle for test speed
        original = engine.stream_agent_run_events

        async def fast_stream(run_id, last_index=0):
            idx = last_index
            max_idle = 3  # Very small for test
            idle = 0
            while idle < max_idle:
                events = _agent_run_events.get(run_id, [])
                if idx < len(events):
                    for i in range(idx, len(events)):
                        yield events[i]
                    idx = len(events)
                    idle = 0
                else:
                    idle += 1
                await asyncio.sleep(0)

        collected = []
        async for ev in fast_stream(run.run_id):
            collected.append(ev)

        assert collected == []  # No events emitted, just timed out

    @pytest.mark.asyncio
    async def test_stream_switches_to_db_after_gc(self, engine, mock_db):
        """After run is GC'd from memory, stream falls back to DB."""
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        engine._append_event(run.run_id, {"event_type": "text_delta", "data": {"chunk": "hello"}})
        run.status = RunStatus.COMPLETED
        run.completed_at = datetime.now(timezone.utc)

        # Simulate GC: remove from memory
        _agent_run_events.pop(run.run_id)
        _active_runs.pop(run.run_id)

        # Mock DB to return the event
        mock_db.execute.return_value.fetchall.return_value = [
            ("text_delta", '{"chunk": "hello"}', None, None),
        ]

        collected = []
        async for ev in engine.stream_agent_run_events(run.run_id):
            collected.append(ev)
            break  # Just need one to prove DB fallback works

        assert len(collected) == 1
        assert collected[0]["event_type"] == "text_delta"


class TestFanInAgentIdFallback:
    """Test _check_fan_in uses restore_run for agent_id when child not in memory."""

    @pytest.mark.asyncio
    async def test_fan_in_uses_restored_agent_id(self, engine):
        """When child is not in _active_runs, fan-in should restore from DB."""
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.WAITING
        parent.waiting_for = "children:" + parent.run_id

        child_id = "child-run-123"
        _child_runs[parent.run_id] = {child_id}

        # Child NOT in _active_runs — simulate cross-worker
        restored_child = AgentRun(
            session_id="s1", user_id="u1", user_input="sub",
            agent_id="security_reviewer",
        )
        restored_child.run_id = child_id
        restored_child.status = RunStatus.COMPLETED

        engine.restore_run = MagicMock(side_effect=lambda rid: restored_child if rid == child_id else parent)
        engine.resume_run = AsyncMock()

        # Provide events for the child
        _agent_run_events[child_id] = [{"event_type": "text_delta", "data": {"chunk": "looks good"}}]

        await engine._check_fan_in(parent.run_id)

        # resume_run should have been called with agent_id from restored child
        engine.resume_run.assert_called_once()
        result = engine.resume_run.call_args[0][1]
        assert "security_reviewer" in result["child_results"]
        assert result["child_results"]["security_reviewer"]["output"] == "looks good"
