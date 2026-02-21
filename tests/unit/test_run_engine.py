"""Tests for RunEngine: start, resume, cancel, timeout, error handling."""

import asyncio
import pytest
from unittest.mock import MagicMock, patch, AsyncMock
from datetime import datetime, timezone

from core.agent.run import AgentRun, RunStatus, RunTrigger
from core.agent.run_engine import (
    RunEngine, _active_runs, _run_events, _run_waiters, _run_tasks,
    _child_runs, _fan_in_tasks, _MAX_RESUME_INPUT_CHARS, _resume_counters,
    _MAX_COMPLETED_RUNS,
)
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
    _fan_in_tasks.clear()
    _resume_counters.clear()
    yield
    _active_runs.clear()
    _run_events.clear()
    _run_waiters.clear()
    _run_tasks.clear()
    _child_runs.clear()
    _fan_in_tasks.clear()
    _resume_counters.clear()


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
                yield StreamEvent(event_type="text_delta", data={"text": "done"})

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
            yield StreamEvent(event_type="text_delta", data={"text": "ok"})

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
    async def test_fan_in_uses_db_when_no_in_memory_children(self, engine):
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
            elif "run_events" in q and "SELECT" in q:
                # _load_events_from_db
                result.fetchall.return_value = [
                    ("text_delta", '{"text":"review done"}', None, "reviewer"),
                ]
            else:
                result.fetchall.return_value = []
                result.fetchone.return_value = None
            return result

        engine.db.execute = MagicMock(side_effect=mock_execute)

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"text": "synthesized"})

        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine._check_fan_in(parent.run_id)

        assert parent.status == RunStatus.COMPLETED

    @pytest.mark.asyncio
    async def test_fan_in_db_returns_no_children(self, engine):
        """If DB also has no children, fan-in should be a no-op."""
        engine.db.execute.return_value.fetchall.return_value = []

        parent = engine.create_run(session_id="s1", user_id="u1", user_input="test")
        parent.status = RunStatus.WAITING

        await engine._check_fan_in(parent.run_id)
        # Should remain waiting — no children found
        assert parent.status == RunStatus.WAITING

    def test_get_child_run_ids_from_db(self, engine):
        engine.db.execute.return_value.fetchall.return_value = [
            ("child-1",), ("child-2",),
        ]
        ids = engine._get_child_run_ids_from_db("parent-1")
        assert ids == {"child-1", "child-2"}

    def test_get_child_run_ids_db_error(self, engine):
        engine.db.execute.side_effect = RuntimeError("db down")
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

    def test_load_config_db_error_logs_warning(self, engine, caplog):
        import logging
        engine.db.execute.side_effect = RuntimeError("connection lost")
        with caplog.at_level(logging.WARNING):
            result = engine._load_agent_config("test-agent")
        assert result is None
        assert "Failed to load agent config" in caplog.text

    def test_load_config_success(self, engine):
        import json
        engine.db.execute.return_value.fetchone.return_value = (
            json.dumps({"system_prompt": "You are helpful"}),
        )
        result = engine._load_agent_config("test-agent")
        assert result == {"system_prompt": "You are helpful"}

    def test_load_config_no_row(self, engine):
        engine.db.execute.return_value.fetchone.return_value = None
        result = engine._load_agent_config("test-agent")
        assert result is None


class TestEventPersistWarning:
    """Test that _append_event logs at warning level on failure."""

    def test_append_event_db_failure_logs_warning(self, engine, caplog):
        import logging
        run = engine.create_run(session_id="s1", user_id="u1", user_input="hi")
        # Make DB write fail
        engine.db.execute.side_effect = RuntimeError("disk full")
        with caplog.at_level(logging.WARNING):
            engine._append_event(run.run_id, {"event_type": "test", "data": {}})
        assert "Event persist failed" in caplog.text
        # Event should still be in local buffer
        assert len(_run_events[run.run_id]) == 1


class TestResumeClaimMultiple:
    """Test that _try_claim_resume works across multiple resume cycles."""

    def _make_engine(self, mock_db):
        with patch.object(RunEngine, '__init__', lambda self, db: setattr(self, 'db', db) or setattr(self, 'event_logger', MagicMock())):
            return RunEngine(mock_db)

    def test_multiple_claims_use_different_idx(self):
        """Adversarial loop: same run resumed multiple times should succeed."""
        db = MagicMock()
        db.execute.return_value = MagicMock()
        engine = self._make_engine(db)

        assert engine._try_claim_resume("run-1") is True
        assert engine._try_claim_resume("run-1") is True
        assert engine._try_claim_resume("run-1") is True

        # Verify different idx values were used
        calls = db.execute.call_args_list
        idxs = []
        for call in calls:
            params = call[0][1] if len(call[0]) > 1 else call[1].get("params", {})
            if isinstance(params, dict) and "idx" in params:
                idxs.append(params["idx"])
        assert idxs == [-1, -2, -3]

    def test_claim_counter_isolated_per_run(self):
        db = MagicMock()
        db.execute.return_value = MagicMock()
        engine = self._make_engine(db)

        engine._try_claim_resume("run-a")
        engine._try_claim_resume("run-b")
        engine._try_claim_resume("run-a")

        assert _resume_counters["run-a"] == 2
        assert _resume_counters["run-b"] == 1


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

    def test_cancel_writes_db_event_for_remote_children(self, engine):
        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.RUNNING

        # Simulate children on another worker (not in _active_runs)
        remote_child_id = "remote-child-123"
        _child_runs[parent.run_id] = {remote_child_id}

        engine.cancel_run(parent.run_id)

        # Should have called create_stream_event for the remote child
        calls = engine.event_logger.create_stream_event.call_args_list
        cancel_calls = [c for c in calls if c[1].get("event_type") == EventType.RUN_CANCELLED.value
                        or (len(c[0]) > 2 and c[0][2] == EventType.RUN_CANCELLED.value)]
        # At least parent + remote child cancel events
        assert len(cancel_calls) >= 2


class TestCausalChainPropagation:
    """Test that child runs inherit parent's causal chain."""

    @pytest.mark.asyncio
    async def test_child_inherits_causal_chain(self, engine):
        parent = engine.create_run(
            session_id="s1", user_id="u1", user_input="review",
            context={"_causal_chain_id": "chain-abc-123"},
        )
        parent.status = RunStatus.RUNNING

        mock_loop = MagicMock()
        mock_loop._current_run_id = None

        async def stream(**kw):
            from core.events.models import StreamEvent
            yield StreamEvent(event_type="text_delta", data={"text": "ok"})

        mock_loop.run_step_stream = stream

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            child = await engine.create_child_run(parent.run_id, "reviewer", "review code")
            await asyncio.sleep(0.1)

        # Child should have inherited the causal chain
        assert child.context.get("_causal_chain_id") == "chain-abc-123"

        # Log events should have been called with causal_chain_id
        log_calls = engine.event_logger.create_stream_event.call_args_list
        child_calls = [c for c in log_calls
                       if c[1].get("metadata", {}).get("run_id") == child.run_id]
        for call in child_calls:
            assert call[1].get("causal_chain_id") == "chain-abc-123"


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
            for i in range(5):
                event_count[0] += 1
                yield StreamEvent(event_type="text_delta", data={"text": f"chunk-{i}"})

        mock_loop.run_step_stream = stream

        # Simulate: DB says child is cancelled after first event
        call_count = [0]
        original_is_cancelled = engine._is_cancelled_in_db

        def mock_is_cancelled(run_id):
            call_count[0] += 1
            return call_count[0] > 1  # Cancel after first check

        engine._is_cancelled_in_db = mock_is_cancelled

        with patch("api.routers.chat._build_chat_loop", return_value=mock_loop):
            await engine.start_run(child)

        assert child.status == RunStatus.CANCELLED
        # Should have stopped early, not consumed all 5 events
        assert event_count[0] < 5
