"""Tests for streaming/chat API endpoints."""

import json
from unittest.mock import AsyncMock, MagicMock, Mock, patch

import pytest
from sqlalchemy.orm import Session

from core.events.models import StreamEvent, StreamEventType


@pytest.fixture
def mock_db():
    """Mock database - SQLAlchemy style."""
    db = Mock(spec=Session)
    mock_result = MagicMock()
    mock_result.first.return_value = MagicMock(session_id="sess_123")
    db.execute.return_value = mock_result
    return db


@pytest.fixture
def mock_auth():
    """Mock authentication."""
    return {"user_id": "user_123", "username": "testuser"}


def _make_mock_loop(stream_events=None, run_step_result=None):
    """Build a mock ChatLoop with given stream events or run_step result."""
    loop = MagicMock()
    loop.set_observer = MagicMock()

    if stream_events is not None:
        async def mock_stream(*a, **kw):
            for e in stream_events:
                yield e
        loop.run_step_stream = mock_stream

    if run_step_result is not None:
        loop.run_step = AsyncMock(return_value=run_step_result)

    return loop


class TestChatStream:
    """Test /chat/stream endpoint."""

    @pytest.mark.asyncio
    @patch("api.routers.chat._build_chat_loop")
    async def test_stream_chat_success(self, mock_build, mock_db, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest

        events = [
            StreamEvent(event_type=StreamEventType.RUN_STARTED, data={"query": "Hello"}, event_id="evt_1", causal_chain_id="chain_1"),
            StreamEvent(event_type=StreamEventType.TEXT_DELTA, data={"chunk": "Hi there"}, event_id="evt_2", causal_chain_id="chain_1"),
            StreamEvent(event_type=StreamEventType.RUN_FINISHED, data={}, event_id="evt_3", causal_chain_id="chain_1"),
        ]
        mock_build.return_value = _make_mock_loop(stream_events=events)

        request = ChatRequest(session_id="sess_123", message="Hello")
        response = await chat_stream(request, mock_auth, mock_db)

        assert response.media_type == "text/event-stream"

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        # First event is session_info, then 3 stream events
        assert collected[0]["event_type"] == "session_info"
        assert collected[1]["event_type"] == "run_started"
        assert collected[2]["event_type"] == "text_delta"
        assert collected[3]["event_type"] == "run_finished"

    @pytest.mark.asyncio
    async def test_stream_chat_session_not_found(self, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest
        from fastapi import HTTPException

        mock_db = MagicMock()
        mock_db.execute.return_value.first.return_value = None

        request = ChatRequest(session_id="nonexistent", message="Hello")
        with pytest.raises(HTTPException) as exc_info:
            await chat_stream(request, mock_auth, mock_db)
        assert exc_info.value.status_code == 404

    @pytest.mark.asyncio
    @patch("api.routers.chat._build_chat_loop")
    async def test_stream_chat_error_handling(self, mock_build, mock_db, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest

        async def error_stream(*a, **kw):
            yield StreamEvent(event_type=StreamEventType.RUN_STARTED, data={}, event_id="evt_1")
            raise Exception("boom")

        loop = MagicMock()
        loop.run_step_stream = error_stream
        loop.set_observer = MagicMock()
        mock_build.return_value = loop

        request = ChatRequest(session_id="sess_123", message="Test")
        response = await chat_stream(request, mock_auth, mock_db)

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        assert collected[-1]["event_type"] == "run_error"

    @pytest.mark.asyncio
    @patch("api.routers.chat._build_chat_loop")
    async def test_stream_chat_tool_calls(self, mock_build, mock_db, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest

        events = [
            StreamEvent(event_type=StreamEventType.RUN_STARTED, data={"query": "Run tests"}, event_id="evt_1"),
            StreamEvent(event_type=StreamEventType.TOOL_CALL_START, data={"tool": "run_tests"}, event_id="evt_2"),
            StreamEvent(event_type=StreamEventType.TOOL_RESULT, data={"result": "ok"}, event_id="evt_3"),
            StreamEvent(event_type=StreamEventType.TEXT_DELTA, data={"chunk": "Done"}, event_id="evt_4"),
            StreamEvent(event_type=StreamEventType.RUN_FINISHED, data={}, event_id="evt_5"),
        ]
        mock_build.return_value = _make_mock_loop(stream_events=events)

        request = ChatRequest(session_id="sess_123", message="Run tests")
        response = await chat_stream(request, mock_auth, mock_db)

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        types = [e["event_type"] for e in collected]
        assert "tool_call_start" in types
        assert "tool_result" in types

    @pytest.mark.asyncio
    @patch("api.routers.chat._build_chat_loop")
    async def test_stream_chat_planning_events(self, mock_build, mock_db, mock_auth):
        from api.routers.chat import chat_stream, ChatRequest

        events = [
            StreamEvent(event_type=StreamEventType.RUN_STARTED, data={}, event_id="evt_1"),
            StreamEvent(event_type=StreamEventType.PLAN_CREATED, data={"plan_id": "p1"}, event_id="evt_2"),
            StreamEvent(event_type=StreamEventType.PLAN_STEP_START, data={"step_id": "s1"}, event_id="evt_3"),
            StreamEvent(event_type=StreamEventType.PLAN_STEP_DONE, data={"step_id": "s1"}, event_id="evt_4"),
            StreamEvent(event_type=StreamEventType.RUN_FINISHED, data={}, event_id="evt_5"),
        ]
        mock_build.return_value = _make_mock_loop(stream_events=events)

        request = ChatRequest(session_id="sess_123", message="Deploy")
        response = await chat_stream(request, mock_auth, mock_db)

        collected = []
        async for chunk in response.body_iterator:
            s = chunk.decode() if isinstance(chunk, bytes) else chunk
            if s.startswith("data: "):
                collected.append(json.loads(s[6:].strip()))

        types = [e["event_type"] for e in collected]
        assert "plan_created" in types
        assert "plan_step_start" in types


class TestChat:
    """Test /chat (non-streaming) endpoint."""

    @pytest.mark.asyncio
    @patch("api.routers.chat._build_chat_loop")
    async def test_chat_success(self, mock_build, mock_db, mock_auth):
        from api.routers.chat import chat, ChatRequest

        mock_build.return_value = _make_mock_loop(run_step_result="Hello back!")

        request = ChatRequest(session_id="sess_123", message="Hello")
        response = await chat(request, mock_auth, mock_db)

        assert response.session_id == "sess_123"
        assert response.message == "Hello back!"

    @pytest.mark.asyncio
    @patch("api.routers.chat._build_chat_loop")
    @patch("api.routers.chat._ensure_session")
    async def test_chat_auto_create_session(self, mock_ensure, mock_build, mock_db, mock_auth):
        from api.routers.chat import chat, ChatRequest

        mock_ensure.return_value = "new_sess_456"
        mock_build.return_value = _make_mock_loop(run_step_result="Hi!")

        request = ChatRequest(message="Hello")  # no session_id
        response = await chat(request, mock_auth, mock_db)

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
