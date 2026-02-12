"""Tests for streaming API endpoints."""

import json
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from core.events.models import StreamEvent, StreamEventType


@pytest.fixture
def mock_db():
    """Mock database."""
    db = MagicMock()
    db.fetchone.return_value = {"session_id": "sess_123"}
    return db


@pytest.fixture
def mock_auth():
    """Mock authentication."""
    return {"user_id": "user_123", "username": "testuser"}


class TestStreamingAPI:
    """Test streaming API endpoints."""

    @pytest.mark.asyncio
    @patch("api.routers.streaming.ChatLoop")
    @patch("api.routers.streaming.SkillSelector")
    @patch("api.routers.streaming.LLMClient")
    @patch("api.routers.streaming.EventLogger")
    async def test_stream_chat_success(
        self, mock_event_logger_class, mock_llm_class, mock_selector_class, mock_chat_loop_class, mock_db, mock_auth
    ):
        """Test successful streaming chat."""
        from api.routers.streaming import stream_chat, StreamChatRequest

        # Mock stream events
        async def mock_stream(*args, **kwargs):
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={"query": "Hello"},
                event_id="evt_1",
                causal_chain_id="chain_1",
            )
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": "Hi there"},
                event_id="evt_2",
                causal_chain_id="chain_1",
            )
            yield StreamEvent(
                event_type=StreamEventType.RUN_FINISHED,
                data={},
                event_id="evt_3",
                causal_chain_id="chain_1",
            )

        mock_chat_loop = MagicMock()
        mock_chat_loop.run_step_stream = mock_stream
        mock_chat_loop_class.return_value = mock_chat_loop

        # Create request
        request = StreamChatRequest(
            session_id="sess_123",
            message="Hello",
        )

        # Call endpoint
        response = await stream_chat(request, mock_auth, mock_db)

        # Verify response is StreamingResponse
        assert response.media_type == "text/event-stream"
        assert response.headers["Cache-Control"] == "no-cache"

        # Collect events
        events = []
        async for chunk in response.body_iterator:
            if chunk.startswith("data: "):
                event_data = json.loads(chunk[6:])
                events.append(event_data)

        # Verify events
        assert len(events) == 3
        assert events[0]["event_type"] == "run_started"
        assert events[0]["data"]["query"] == "Hello"
        assert events[1]["event_type"] == "text_delta"
        assert events[1]["data"]["chunk"] == "Hi there"
        assert events[2]["event_type"] == "run_finished"

    @pytest.mark.asyncio
    async def test_stream_chat_session_not_found(self, mock_auth):
        """Test streaming with non-existent session."""
        from api.routers.streaming import stream_chat, StreamChatRequest

        # Mock DB with no session
        mock_db = MagicMock()
        mock_db.fetchone.return_value = None

        # Create request
        request = StreamChatRequest(
            session_id="nonexistent",
            message="Hello",
        )

        # Call endpoint - should raise HTTPException
        from fastapi import HTTPException

        with pytest.raises(HTTPException) as exc_info:
            await stream_chat(request, mock_auth, mock_db)

        assert exc_info.value.status_code == 404
        assert "not found" in exc_info.value.detail.lower()

    @pytest.mark.asyncio
    @patch("api.routers.streaming.ChatLoop")
    @patch("api.routers.streaming.SkillSelector")
    @patch("api.routers.streaming.LLMClient")
    @patch("api.routers.streaming.EventLogger")
    async def test_stream_chat_with_context(
        self, mock_event_logger_class, mock_llm_class, mock_selector_class, mock_chat_loop_class, mock_db, mock_auth
    ):
        """Test streaming with custom context."""
        from api.routers.streaming import stream_chat, StreamChatRequest

        async def mock_stream(*args, **kwargs):
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={"query": "Test"},
                event_id="evt_1",
            )

        mock_chat_loop = MagicMock()
        mock_chat_loop.run_step_stream = mock_stream
        mock_chat_loop_class.return_value = mock_chat_loop

        # Create request with context
        request = StreamChatRequest(
            session_id="sess_123",
            message="Test",
            context={"key": "value"},
            max_candidates=10,
        )

        # Call endpoint
        response = await stream_chat(request, mock_auth, mock_db)

        # Verify response
        assert response.media_type == "text/event-stream"

        # Verify ChatLoop was instantiated
        mock_chat_loop_class.assert_called_once()

    @pytest.mark.asyncio
    @patch("api.routers.streaming.ChatLoop")
    @patch("api.routers.streaming.SkillSelector")
    @patch("api.routers.streaming.LLMClient")
    @patch("api.routers.streaming.EventLogger")
    async def test_stream_chat_error_handling(
        self, mock_event_logger_class, mock_llm_class, mock_selector_class, mock_chat_loop_class, mock_db, mock_auth
    ):
        """Test error handling during streaming."""
        from api.routers.streaming import stream_chat, StreamChatRequest

        async def mock_stream(*args, **kwargs):
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={"query": "Test"},
                event_id="evt_1",
            )
            raise Exception("Stream error")

        mock_chat_loop = MagicMock()
        mock_chat_loop.run_step_stream = mock_stream
        mock_chat_loop_class.return_value = mock_chat_loop

        # Create request
        request = StreamChatRequest(
            session_id="sess_123",
            message="Test",
        )

        # Call endpoint
        response = await stream_chat(request, mock_auth, mock_db)

        # Collect events
        events = []
        async for chunk in response.body_iterator:
            if chunk.startswith("data: "):
                event_data = json.loads(chunk[6:])
                events.append(event_data)

        # Should have run_started and run_error
        assert len(events) >= 2
        assert events[0]["event_type"] == "run_started"
        assert events[-1]["event_type"] == "run_error"
        assert "error" in events[-1]["data"]

    @pytest.mark.asyncio
    @patch("api.routers.streaming.ChatLoop")
    @patch("api.routers.streaming.SkillSelector")
    @patch("api.routers.streaming.LLMClient")
    @patch("api.routers.streaming.EventLogger")
    async def test_stream_chat_tool_calls(
        self, mock_event_logger_class, mock_llm_class, mock_selector_class, mock_chat_loop_class, mock_db, mock_auth
    ):
        """Test streaming with tool calls."""
        from api.routers.streaming import stream_chat, StreamChatRequest

        async def mock_stream(*args, **kwargs):
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={"query": "Run tests"},
                event_id="evt_1",
            )
            yield StreamEvent(
                event_type=StreamEventType.TOOL_CALL_START,
                data={"tool": "run_tests", "call_id": "tc_1"},
                event_id="evt_2",
            )
            yield StreamEvent(
                event_type=StreamEventType.TOOL_RESULT,
                data={"call_id": "tc_1", "result": "Tests passed"},
                event_id="evt_3",
            )
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": "All tests passed"},
                event_id="evt_4",
            )
            yield StreamEvent(
                event_type=StreamEventType.RUN_FINISHED,
                data={},
                event_id="evt_5",
            )

        mock_chat_loop = MagicMock()
        mock_chat_loop.run_step_stream = mock_stream
        mock_chat_loop_class.return_value = mock_chat_loop

        # Create request
        request = StreamChatRequest(
            session_id="sess_123",
            message="Run tests",
        )

        # Call endpoint
        response = await stream_chat(request, mock_auth, mock_db)

        # Collect events
        events = []
        async for chunk in response.body_iterator:
            if chunk.startswith("data: "):
                event_data = json.loads(chunk[6:])
                events.append(event_data)

        # Verify event sequence
        assert len(events) == 5
        assert events[0]["event_type"] == "run_started"
        assert events[1]["event_type"] == "tool_call_start"
        assert events[2]["event_type"] == "tool_result"
        assert events[3]["event_type"] == "text_delta"
        assert events[4]["event_type"] == "run_finished"

    @pytest.mark.asyncio
    @patch("api.routers.streaming.ChatLoop")
    @patch("api.routers.streaming.SkillSelector")
    @patch("api.routers.streaming.LLMClient")
    @patch("api.routers.streaming.EventLogger")
    async def test_stream_chat_planning_events(
        self, mock_event_logger_class, mock_llm_class, mock_selector_class, mock_chat_loop_class, mock_db, mock_auth
    ):
        """Test streaming with planning events."""
        from api.routers.streaming import stream_chat, StreamChatRequest

        async def mock_stream(*args, **kwargs):
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={"query": "Deploy app"},
                event_id="evt_1",
            )
            yield StreamEvent(
                event_type=StreamEventType.PLAN_CREATED,
                data={"plan_id": "plan_1", "steps": 3},
                event_id="evt_2",
            )
            yield StreamEvent(
                event_type=StreamEventType.PLAN_STEP_START,
                data={"step_id": "step_1", "description": "Run tests"},
                event_id="evt_3",
            )
            yield StreamEvent(
                event_type=StreamEventType.PLAN_STEP_DONE,
                data={"step_id": "step_1", "status": "completed"},
                event_id="evt_4",
            )
            yield StreamEvent(
                event_type=StreamEventType.RUN_FINISHED,
                data={},
                event_id="evt_5",
            )

        mock_chat_loop = MagicMock()
        mock_chat_loop.run_step_stream = mock_stream
        mock_chat_loop_class.return_value = mock_chat_loop

        # Create request
        request = StreamChatRequest(
            session_id="sess_123",
            message="Deploy app",
        )

        # Call endpoint
        response = await stream_chat(request, mock_auth, mock_db)

        # Collect events
        events = []
        async for chunk in response.body_iterator:
            if chunk.startswith("data: "):
                event_data = json.loads(chunk[6:])
                events.append(event_data)

        # Verify planning events
        assert len(events) == 5
        assert events[0]["event_type"] == "run_started"
        assert events[1]["event_type"] == "plan_created"
        assert events[2]["event_type"] == "plan_step_start"
        assert events[3]["event_type"] == "plan_step_done"
        assert events[4]["event_type"] == "run_finished"

