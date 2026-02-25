"""Tests for three review-point fixes:

1. Parallel delegation + non-delegate tools: mixed tool_calls batch executes ALL tools
2. Event field consistency: RunEngine fan-in reads 'chunk' (matching ChatLoop output)
3. Evaluation/Learning API auth: all endpoints require authentication

These tests verify the fixes at the integration level.
"""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch
from sqlalchemy.orm import Session

import pytest
from fastapi.testclient import TestClient

from api.main import app
from core.utils.id_generator import generate_id


# ============================================================================
# Fixtures
# ============================================================================

@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def auth_headers(client):
    username = f"rv_{generate_id()[:8]}"
    client.post("/auth/register", json={
        "username": username,
        "email": f"{username}@test.com",
        "password": "testpass1234",
    })
    resp = client.post("/auth/login", json={
        "username": username, "password": "testpass1234",
    })
    return {"Authorization": f"Bearer {resp.json()['access_token']}"}


# ============================================================================
# Fix 1: Parallel delegation executes non-delegate tools too
# ============================================================================

class TestParallelDelegationMixedTools:
    """When tool_calls contains delegate_task + other tools, all must execute."""

    def test_parallel_branch_includes_other_calls_loop(self):
        """Verify the parallel delegation branch has code to execute non-delegate tools.

        This is a structural test: the old code only iterated delegation_calls,
        silently dropping other tools. The fix adds an other_calls loop.
        """
        import inspect
        from core.agent.chat_loop import ChatLoop

        source = inspect.getsource(ChatLoop.run_step_stream)
        # The fix: after parallel delegation, there must be an other_calls loop
        assert "other_calls" in source, (
            "Parallel delegation branch must execute non-delegate tools via other_calls"
        )
        assert '_execute_single_tool' in source, (
            "Both branches must use _execute_single_tool"
        )

    @pytest.mark.asyncio
    async def test_execute_single_tool_works(self):
        """_execute_single_tool executes a tool and yields TOOL_CALL_START + TOOL_RESULT."""
        from core.agent.chat_loop import ChatLoop
        from core.events.models import StreamEvent, StreamEventType

        mock_event_logger = MagicMock()
        mock_event_logger.create_stream_event.return_value = MagicMock(
            event_id="evt1", causal_chain_id="chain1",
        )

        loop = ChatLoop.__new__(ChatLoop)
        loop.event_logger = mock_event_logger
        loop.executor = MagicMock()
        loop.executor.execute_skill_with_feedback.return_value = {"result": "file content"}
        loop.agent_id = "test-agent"
        loop.hitl_policy = None
        loop.mcp_bridge = None
        loop.scratchpad = None
        loop._last_selection_event_id = None
        loop._pipeline = MagicMock()
        loop._evaluate_hitl = MagicMock(return_value=(True, ""))
        loop.llm = MagicMock()

        # Mock CoT audit to always pass
        with patch("core.verification.cot_audit.audit_tool_call") as mock_audit:
            mock_audit.return_value = MagicMock(safe=True)

            tc = {"id": "c1", "function": {"name": "read_file", "arguments": '{"path": "/x"}'}}
            user_event = MagicMock(event_id="evt0", causal_chain_id="chain1")
            messages = []

            events = []
            async for evt in loop._execute_single_tool(
                tc, "read_file", "u1", "s1", "query", "reasoning",
                user_event, messages,
            ):
                events.append(evt)

        event_types = [e.event_type for e in events]
        assert StreamEventType.TOOL_CALL_START in event_types
        assert StreamEventType.TOOL_RESULT in event_types
        # Tool result should be appended to messages
        assert any(m.get("role") == "tool" for m in messages)


# ============================================================================
# Fix 2: Event field consistency — fan-in reads 'chunk'
# ============================================================================

class TestEventFieldConsistency:
    """RunEngine fan-in must read 'chunk' key from TEXT_DELTA events."""

    @pytest.fixture(autouse=True)
    def clean_state(self):
        from core.agent.run_engine import (
            _active_runs, _run_events, _child_runs, _run_waiters, _run_tasks,
            cleanup_fan_in_tasks,
        )
        _active_runs.clear()
        _run_events.clear()
        _child_runs.clear()
        _run_waiters.clear()
        _run_tasks.clear()
        cleanup_fan_in_tasks()
        yield
        _active_runs.clear()
        _run_events.clear()
        _child_runs.clear()
        _run_waiters.clear()
        _run_tasks.clear()
        cleanup_fan_in_tasks()

    @pytest.fixture
    def engine(self):
        from core.agent.run_engine import RunEngine
        from tests.conftest import make_run_engine_mock_init
        mock_db = MagicMock(spec=Session)
        with patch.object(RunEngine, '__init__', make_run_engine_mock_init()):
            e = RunEngine(lambda: mock_db)
            e._try_claim_resume = MagicMock(return_value=True)
            return e

    @pytest.mark.asyncio
    async def test_fan_in_reads_chunk_key(self, engine):
        """_check_fan_in collects child output from data['chunk'], not data['text']."""
        from core.agent.run_engine import (
            _active_runs, _run_events, _child_runs, AgentRun, RunStatus,
        )

        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review all")
        parent.status = RunStatus.WAITING
        parent.waiting_for = f"children:{parent.run_id}"

        child_id = generate_id()
        _child_runs[parent.run_id] = {child_id}

        child = AgentRun(session_id="s1", user_id="u1", user_input="sub-task")
        child.run_id = child_id
        child.status = RunStatus.COMPLETED
        child.agent_id = "reviewer_a"
        _active_runs[child_id] = child

        # Child events use 'chunk' key (matching real ChatLoop output)
        _run_events[child_id] = [
            {"event_type": "text_delta", "data": {"chunk": "All looks "}},
            {"event_type": "text_delta", "data": {"chunk": "good!"}},
        ]

        engine.resume_run = AsyncMock()

        await engine._check_fan_in(parent.run_id)

        engine.resume_run.assert_called_once()
        result = engine.resume_run.call_args[0][1]
        assert result["child_results"]["reviewer_a"]["output"] == "All looks good!"

    @pytest.mark.asyncio
    async def test_fan_in_old_text_key_gives_empty(self, engine):
        """Verify that old 'text' key would NOT be collected (regression guard)."""
        from core.agent.run_engine import (
            _active_runs, _run_events, _child_runs, AgentRun, RunStatus,
        )

        parent = engine.create_run(session_id="s1", user_id="u1", user_input="review")
        parent.status = RunStatus.WAITING
        parent.waiting_for = f"children:{parent.run_id}"

        child_id = generate_id()
        _child_runs[parent.run_id] = {child_id}

        child = AgentRun(session_id="s1", user_id="u1", user_input="sub")
        child.run_id = child_id
        child.status = RunStatus.COMPLETED
        child.agent_id = "agent_x"
        _active_runs[child_id] = child

        # Simulate OLD format with 'text' key — should NOT be collected
        _run_events[child_id] = [
            {"event_type": "text_delta", "data": {"text": "this should be lost"}},
        ]

        engine.resume_run = AsyncMock()
        await engine._check_fan_in(parent.run_id)

        result = engine.resume_run.call_args[0][1]
        assert result["child_results"]["agent_x"]["output"] == "(no text output)"


# ============================================================================
# Fix 3: Evaluation + Learning API auth enforcement
# ============================================================================

class TestEvaluationAuthEnforcement:
    """All evaluation GET endpoints now require authentication."""

    @pytest.mark.parametrize("path", [
        "/api/v1/evaluation/quality/trend",
        "/api/v1/evaluation/drift",
        "/api/v1/evaluation/gates",
        "/api/v1/evaluation/calibration",
        "/api/v1/evaluation/sessions/scores",
    ])
    def test_evaluation_get_requires_auth(self, client, path):
        resp = client.get(path)
        assert resp.status_code == 401, f"GET {path} should require auth"

    @pytest.mark.parametrize("path", [
        "/api/v1/evaluation/quality/trend",
        "/api/v1/evaluation/drift",
        "/api/v1/evaluation/gates",
        "/api/v1/evaluation/calibration",
        "/api/v1/evaluation/sessions/scores",
    ])
    def test_evaluation_get_with_auth(self, client, auth_headers, path):
        resp = client.get(path, headers=auth_headers)
        assert resp.status_code == 200, f"GET {path} with auth should succeed"


class TestLearningAuthEnforcement:
    """All learning endpoints (except health) now require authentication."""

    @pytest.mark.parametrize("method,path,json_body", [
        ("GET", "/api/v1/learning/signals", None),
        ("GET", "/api/v1/learning/stats", None),
        ("POST", "/api/v1/learning/trigger", {"days": 7}),
        ("POST", "/api/v1/learning/feedback", {"event_id": "x", "feedback_type": "wrong_skill"}),
    ])
    def test_learning_requires_auth(self, client, method, path, json_body):
        if method == "GET":
            resp = client.get(path)
        else:
            resp = client.post(path, json=json_body)
        assert resp.status_code == 401, f"{method} {path} should require auth"

    def test_learning_health_no_auth(self, client):
        """Health check should remain unauthenticated."""
        resp = client.get("/api/v1/learning/health")
        assert resp.status_code == 200

    def test_learning_signals_with_auth(self, client, auth_headers):
        resp = client.get("/api/v1/learning/signals", headers=auth_headers)
        assert resp.status_code == 200
        data = resp.json()
        assert "wrong_skill" in data["signal_types"]

    def test_learning_stats_with_auth(self, client, auth_headers):
        resp = client.get("/api/v1/learning/stats", headers=auth_headers)
        assert resp.status_code == 200
        assert "total_learnings" in resp.json()
