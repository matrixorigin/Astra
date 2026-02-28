"""Tests for streaming/chat API endpoints."""

import json
from unittest.mock import AsyncMock, MagicMock, patch

import pytest


@pytest.fixture
def mock_auth():
    """Mock authentication."""
    return {"user_id": "user_123", "username": "testuser"}


class TestChatStream:
    """Test /chat/stream endpoint."""

    @pytest.mark.asyncio
    @patch("api.routers.chat._ensure_session", return_value="sess_123")
    @patch("api.routers.chat._get_engine")
    async def test_stream_chat_success(self, mock_get_engine, mock_ensure, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest
        from core.agent.run import AgentRun

        mock_engine = MagicMock()
        mock_run = AgentRun(session_id="sess_123", user_id="user_123", user_input="Hello")
        mock_engine.create_run.return_value = mock_run
        mock_engine.start_run = AsyncMock()

        async def mock_stream(run_id, **kw):
            yield {"event_type": "text_delta", "data": {"chunk": "Hi there"}}
            yield {"event_type": "run_finished", "data": {}}

        mock_engine.stream_agent_run_events = mock_stream
        mock_get_engine.return_value = mock_engine

        request = ChatRequest(session_id="sess_123", message="Hello")
        response = await chat_stream(request, mock_auth)

        assert response.media_type == "text/event-stream"

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        assert collected[0]["event_type"] == "session_info"
        assert collected[1]["event_type"] == "text_delta"
        assert collected[2]["event_type"] == "run_finished"

    @pytest.mark.asyncio
    async def test_stream_chat_session_not_found(self, mock_auth):
        """Session not found → SSE error event inside the stream (not HTTPException)."""
        from api.routers.chat import chat_stream, ChatRequest
        from sqlalchemy.orm import Session

        mock_db = MagicMock(spec=Session)
        mock_db.execute.return_value.first.return_value = None

        request = ChatRequest(session_id="nonexistent", message="Hello")
        with patch("api.routers.chat.SessionLocal", return_value=mock_db):
            response = await chat_stream(request, mock_auth)

        assert response.media_type == "text/event-stream"

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        err = [e for e in collected if e.get("type") == "error"]
        assert len(err) >= 1
        assert "not found" in err[0]["message"].lower()

    @pytest.mark.asyncio
    @patch("api.routers.chat._ensure_session", return_value="sess_123")
    @patch("api.routers.chat._get_engine")
    async def test_stream_chat_error_handling(self, mock_get_engine, mock_ensure, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest

        mock_engine = MagicMock()
        mock_run = MagicMock()
        mock_run.run_id = "run_err"
        mock_engine.create_run.return_value = mock_run
        mock_engine.start_run = AsyncMock()

        async def error_stream(run_id, **kw):
            yield {"event_type": "text_delta", "data": {"chunk": "partial"}}
            raise Exception("boom")

        mock_engine.stream_agent_run_events = error_stream
        mock_get_engine.return_value = mock_engine

        request = ChatRequest(session_id="sess_123", message="Test")
        response = await chat_stream(request, mock_auth)

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        err = [e for e in collected if e.get("type") == "error"]
        assert len(err) >= 1

    @pytest.mark.asyncio
    @patch("api.routers.chat._ensure_session", return_value="sess_123")
    @patch("api.routers.chat._get_engine")
    async def test_stream_chat_tool_calls(self, mock_get_engine, mock_ensure, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest

        mock_engine = MagicMock()
        mock_run = MagicMock()
        mock_run.run_id = "run_tc"
        mock_engine.create_run.return_value = mock_run
        mock_engine.start_run = AsyncMock()

        async def mock_stream(run_id, **kw):
            yield {"event_type": "tool_call_start", "data": {"tool": "run_tests"}}
            yield {"event_type": "tool_result", "data": {"result": "ok"}}
            yield {"event_type": "text_delta", "data": {"chunk": "Done"}}
            yield {"event_type": "run_finished", "data": {}}

        mock_engine.stream_agent_run_events = mock_stream
        mock_get_engine.return_value = mock_engine

        request = ChatRequest(session_id="sess_123", message="Run tests")
        response = await chat_stream(request, mock_auth)

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        types = [e["event_type"] for e in collected]
        assert "tool_call_start" in types
        assert "tool_result" in types

    @pytest.mark.asyncio
    @patch("api.routers.chat._ensure_session", return_value="sess_123")
    @patch("api.routers.chat._get_engine")
    async def test_stream_chat_planning_events(self, mock_get_engine, mock_ensure, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest

        mock_engine = MagicMock()
        mock_run = MagicMock()
        mock_run.run_id = "run_plan"
        mock_engine.create_run.return_value = mock_run
        mock_engine.start_run = AsyncMock()

        async def mock_stream(run_id, **kw):
            yield {"event_type": "plan_created", "data": {"plan_id": "p1"}}
            yield {"event_type": "plan_step_start", "data": {"step_id": "s1"}}
            yield {"event_type": "plan_step_done", "data": {"step_id": "s1"}}
            yield {"event_type": "run_finished", "data": {}}

        mock_engine.stream_agent_run_events = mock_stream
        mock_get_engine.return_value = mock_engine

        request = ChatRequest(session_id="sess_123", message="Deploy")
        response = await chat_stream(request, mock_auth)

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        types = [e["event_type"] for e in collected]
        assert "plan_created" in types
        assert "plan_step_start" in types

    @pytest.mark.asyncio
    @patch("api.routers.chat._ensure_session", return_value="sess_123")
    @patch("api.routers.chat._get_engine")
    async def test_stream_chat_reasoning_events(self, mock_get_engine, mock_ensure, mock_auth):
        """Reasoning (CoT) events are forwarded through the SSE stream."""
        from api.routers.chat import chat_stream, ChatRequest

        mock_engine = MagicMock()
        mock_run = MagicMock()
        mock_run.run_id = "run_reason"
        mock_engine.create_run.return_value = mock_run
        mock_engine.start_run = AsyncMock()

        async def mock_stream(run_id, **kw):
            yield {"event_type": "reasoning_message_content", "data": {"content": "Let me think..."}}
            yield {"event_type": "text_delta", "data": {"chunk": "Answer"}}
            yield {"event_type": "run_finished", "data": {}}

        mock_engine.stream_agent_run_events = mock_stream
        mock_get_engine.return_value = mock_engine

        request = ChatRequest(session_id="sess_123", message="Think about this")
        response = await chat_stream(request, mock_auth)

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        types = [e["event_type"] for e in collected]
        assert "reasoning_message_content" in types
        reasoning_evt = [e for e in collected if e["event_type"] == "reasoning_message_content"][0]
        assert reasoning_evt["data"]["content"] == "Let me think..."


class TestChat:
    """Test /chat (non-streaming) endpoint."""

    @pytest.mark.asyncio
    @patch("api.routers.chat._ensure_session", return_value="sess_123")
    @patch("api.routers.chat._get_engine")
    async def test_chat_success(self, mock_get_engine, mock_ensure, mock_auth):
        from api.routers.chat import chat, ChatRequest
        from core.agent.run import AgentRun, RunStatus

        mock_engine = MagicMock()
        mock_run = AgentRun(session_id="sess_123", user_id="user_123", user_input="Hello")
        mock_engine.create_run.return_value = mock_run
        mock_engine.start_run = AsyncMock()
        mock_get_engine.return_value = mock_engine

        request = ChatRequest(session_id="sess_123", message="Hello")
        response = await chat(request, mock_auth)

        assert response.session_id == "sess_123"
        assert response.run_id == mock_run.run_id
        assert response.status == "pending"

    @pytest.mark.asyncio
    @patch("api.routers.chat._get_engine")
    @patch("api.routers.chat._ensure_session")
    async def test_chat_auto_create_session(self, mock_ensure, mock_get_engine, mock_auth):
        from api.routers.chat import chat, ChatRequest
        from core.agent.run import AgentRun

        mock_ensure.return_value = "new_sess_456"
        mock_engine = MagicMock()
        mock_run = AgentRun(session_id="new_sess_456", user_id="user_123", user_input="Hello")
        mock_engine.create_run.return_value = mock_run
        mock_engine.start_run = AsyncMock()
        mock_get_engine.return_value = mock_engine

        request = ChatRequest(message="Hello")
        response = await chat(request, mock_auth)

        assert response.session_id == "new_sess_456"
        mock_ensure.assert_called_once()


class TestEnsureSession:
    """Test _ensure_session with real SessionManager (no mock)."""

    @pytest.mark.asyncio
    @patch("core.events.session_manager.SessionManager")
    async def test_auto_create_calls_create_session_correctly(self, mock_mgr_class):
        # Re-import so the local import inside _ensure_session picks up the patched class
        from api.routers.chat import _ensure_session

        mock_session = MagicMock(session_id="new_123")
        mock_mgr_class.return_value.create_session.return_value = mock_session

        db = MagicMock()
        result = _ensure_session(db, "user_1", None, "agent_1")

        assert result == "new_123"
        mock_mgr_class.return_value.create_session.assert_called_once_with(
            user_id="user_1", metadata={"agent_id": "agent_1"}
        )

    @pytest.mark.asyncio
    @patch("core.events.session_manager.SessionManager")
    async def test_auto_create_no_agent_id(self, mock_mgr_class):
        from api.routers.chat import _ensure_session

        mock_session = MagicMock(session_id="new_456")
        mock_mgr_class.return_value.create_session.return_value = mock_session

        db = MagicMock()
        result = _ensure_session(db, "user_1", None, None)

        assert result == "new_456"
        mock_mgr_class.return_value.create_session.assert_called_once_with(
            user_id="user_1", metadata=None
        )
