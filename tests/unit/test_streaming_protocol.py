"""Tests for streaming protocol implementation.

Tests stream persistence, replay, validation, and multi-agent multiplexing.
"""

import json
from datetime import datetime, timezone

import pytest

from core.agent.stream_multiplexer import StreamMultiplexer
from core.agent.stream_persistence import StreamPersistence
from core.agent.stream_replay import StreamReplay
from core.agent.stream_validator import StreamValidator
from core.events.event_logger import EventLogger
from core.events.models import StreamEvent, StreamEventType


class TestStreamPersistence:
    """Test stream persistence layer."""

    @pytest.mark.asyncio
    async def test_persist_stream(self):
        """Test that stream events are persisted to database."""
        from unittest.mock import MagicMock

        mock_logger = MagicMock()
        mock_logger.create_stream_event = MagicMock(return_value=MagicMock(event_id="evt1"))

        persistence = StreamPersistence(mock_logger)

        # Create test stream
        async def test_stream():
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={"query": "test"},
                agent_id="test-agent",
            )
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": "Hello"},
                agent_id="test-agent",
            )
            yield StreamEvent(
                event_type=StreamEventType.RUN_FINISHED,
                data={},
                agent_id="test-agent",
            )

        # Persist stream
        events = []
        async for event in persistence.persist_stream(
            stream=test_stream(),
            user_id="user1",
            session_id="session1",
            agent_id="test-agent",
            agent_version="1.0.0",
            causal_chain_id="chain1",
        ):
            events.append(event)

        # Verify all events yielded
        assert len(events) == 3
        assert events[0].event_type == StreamEventType.RUN_STARTED
        assert events[1].event_type == StreamEventType.TEXT_DELTA
        assert events[2].event_type == StreamEventType.RUN_FINISHED

        # Verify logger called for each event
        assert mock_logger.create_stream_event.call_count == 3

    @pytest.mark.asyncio
    async def test_persist_stream_with_error(self):
        """Test that stream continues even if logging fails."""
        from unittest.mock import MagicMock

        mock_logger = MagicMock()
        mock_logger.create_stream_event = MagicMock(side_effect=Exception("DB error"))

        persistence = StreamPersistence(mock_logger)

        # Create test stream
        async def test_stream():
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={"query": "test"},
            )

        # Should not raise even if logging fails
        events = []
        async for event in persistence.persist_stream(
            stream=test_stream(),
            user_id="user1",
            session_id="session1",
            agent_id="test-agent",
            agent_version="1.0.0",
            causal_chain_id="chain1",
        ):
            events.append(event)

        assert len(events) == 1


class TestStreamReplay:
    """Test stream replay engine."""

    @pytest.mark.asyncio
    async def test_replay_stream(self):
        """Test replaying stream from logged events."""
        from unittest.mock import MagicMock
        from api.models import Event

        # Mock database with events
        mock_db = MagicMock()
        mock_result = MagicMock()

        # Create mock events
        event1 = Event(
            event_id="evt1",
            user_id="user1",
            session_id="session1",
            agent_id="test-agent",
            agent_version="1.0.0",
            event_type="stream_run_started",
            content=json.dumps(
                {
                    "event_type": "run_started",
                    "data": {"query": "test"},
                    "stream_event_id": "evt1",
                }
            ),
            causal_chain_id="chain1",
            event_metadata={"agent_id": "test-agent"},
        )
        event2 = Event(
            event_id="evt2",
            user_id="user1",
            session_id="session1",
            agent_id="test-agent",
            agent_version="1.0.0",
            event_type="stream_text_delta",
            content=json.dumps(
                {
                    "event_type": "text_delta",
                    "data": {"chunk": "Hello"},
                    "stream_event_id": "evt2",
                }
            ),
            causal_chain_id="chain1",
            event_metadata={"agent_id": "test-agent"},
        )

        mock_scalars = MagicMock()
        mock_scalars.all.return_value = [event1, event2]
        mock_result.scalars.return_value = mock_scalars
        mock_db.execute.return_value = mock_result

        # Replay
        replay = StreamReplay(lambda: mock_db)
        events = []
        async for event in replay.replay_stream("session1", "chain1"):
            events.append(event)

        # Verify
        assert len(events) == 2
        assert events[0].event_type == StreamEventType.RUN_STARTED
        assert events[1].event_type == StreamEventType.TEXT_DELTA

    @pytest.mark.asyncio
    async def test_replay_stream_at_timestamp(self):
        """Test time-travel replay up to specific timestamp."""
        from unittest.mock import MagicMock
        from api.models import Event

        mock_db = MagicMock()
        mock_result = MagicMock()

        event1 = Event(
            event_id="evt1",
            user_id="user1",
            session_id="session1",
            agent_id="test-agent",
            agent_version="1.0.0",
            event_type="stream_run_started",
            content=json.dumps(
                {
                    "event_type": "run_started",
                    "data": {},
                    "stream_event_id": "evt1",
                }
            ),
            causal_chain_id="chain1",
            event_metadata={"agent_id": "test-agent"},
        )

        mock_scalars = MagicMock()
        mock_scalars.all.return_value = [event1]
        mock_result.scalars.return_value = mock_scalars
        mock_db.execute.return_value = mock_result

        # Replay up to timestamp
        replay = StreamReplay(lambda: mock_db)
        events = []
        async for event in replay.replay_stream_at(
            "session1", datetime.now(timezone.utc), "chain1"
        ):
            events.append(event)

        assert len(events) == 1

    def test_get_stream_state_at(self):
        """Test getting stream state at specific timestamp."""
        from unittest.mock import MagicMock
        from api.models import Event

        mock_db = MagicMock()
        mock_result = MagicMock()

        event1 = Event(
            event_id="evt1",
            user_id="user1",
            session_id="session1",
            agent_id="test-agent",
            agent_version="1.0.0",
            event_type="stream_run_started",
            content=json.dumps(
                {
                    "event_type": "run_started",
                    "data": {},
                    "stream_event_id": "evt1",
                }
            ),
            causal_chain_id="chain1",
            event_metadata={"agent_id": "test-agent"},
        )
        event1.created_at = datetime.now(timezone.utc)

        event2 = Event(
            event_id="evt2",
            user_id="user1",
            session_id="session1",
            agent_id="test-agent",
            agent_version="1.0.0",
            event_type="stream_text_delta",
            content=json.dumps(
                {
                    "event_type": "text_delta",
                    "data": {"delta": "Hello"},
                    "stream_event_id": "evt2",
                }
            ),
            causal_chain_id="chain1",
            event_metadata={"agent_id": "test-agent"},
        )
        event2.created_at = datetime.now(timezone.utc)

        mock_scalars = MagicMock()
        mock_scalars.all.return_value = [event1, event2]
        mock_result.scalars.return_value = mock_scalars
        mock_db.execute.return_value = mock_result

        # Get state
        replay = StreamReplay(lambda: mock_db)
        state = replay.get_stream_state_at("session1", datetime.now(timezone.utc), "chain1")

        # Verify state
        assert state["session_id"] == "session1"
        assert state["status"] == "running"
        assert "Hello" in state["text_accumulated"]


class TestStreamValidator:
    """Test stream validator."""

    @pytest.mark.asyncio
    async def test_valid_stream_sequence(self):
        """Test validation of valid AG-UI protocol sequence."""
        validator = StreamValidator()

        # Create valid stream
        async def valid_stream():
            yield StreamEvent(event_type=StreamEventType.RUN_STARTED, data={})
            yield StreamEvent(event_type=StreamEventType.TEXT_DELTA, data={"chunk": "Hello"})
            yield StreamEvent(event_type=StreamEventType.TEXT_DONE, data={})
            yield StreamEvent(event_type=StreamEventType.RUN_FINISHED, data={})

        # Validate
        events = []
        async for event in validator.validate_stream(valid_stream()):
            events.append(event)

        # Check report
        report = validator.get_report()
        assert report["is_valid"]
        assert len(report["violations"]) == 0
        assert report["total_events"] == 4

    @pytest.mark.asyncio
    async def test_invalid_stream_sequence(self):
        """Test detection of protocol violations."""
        validator = StreamValidator()

        # Create invalid stream (TEXT_DELTA before RUN_STARTED)
        async def invalid_stream():
            yield StreamEvent(event_type=StreamEventType.TEXT_DELTA, data={"chunk": "Hello"})
            yield StreamEvent(event_type=StreamEventType.RUN_STARTED, data={})

        # Validate
        events = []
        async for event in validator.validate_stream(invalid_stream()):
            events.append(event)

        # Check report
        report = validator.get_report()
        assert not report["is_valid"]
        assert len(report["violations"]) > 0

    @pytest.mark.asyncio
    async def test_tool_call_sequence(self):
        """Test validation of tool call sequence."""
        validator = StreamValidator()

        # Create stream with tool calls
        async def tool_stream():
            yield StreamEvent(event_type=StreamEventType.RUN_STARTED, data={})
            yield StreamEvent(
                event_type=StreamEventType.TOOL_CALL_START,
                data={"tool": "test_tool"},
            )
            yield StreamEvent(event_type=StreamEventType.TOOL_CALL_ARGS, data={"args": "{}"})
            yield StreamEvent(event_type=StreamEventType.TOOL_CALL_END, data={})
            yield StreamEvent(event_type=StreamEventType.TOOL_RESULT, data={"result": "ok"})
            yield StreamEvent(event_type=StreamEventType.RUN_FINISHED, data={})

        # Validate
        events = []
        async for event in validator.validate_stream(tool_stream()):
            events.append(event)

        # Check report
        report = validator.get_report()
        assert report["is_valid"]


class TestStreamMultiplexer:
    """Test stream multiplexer for multi-agent coordination."""

    @pytest.mark.asyncio
    async def test_merge_multiple_streams(self):
        """Test merging streams from multiple agents."""
        multiplexer = StreamMultiplexer()

        # Create test streams
        async def agent1_stream():
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": "Agent1"},
                agent_id="agent1",
            )

        async def agent2_stream():
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DELTA,
                data={"chunk": "Agent2"},
                agent_id="agent2",
            )

        # Merge
        streams = {"agent1": agent1_stream(), "agent2": agent2_stream()}
        events = []
        async for event in multiplexer.merge_streams(streams):
            events.append(event)

        # Verify both agents' events present
        assert len(events) == 2
        agent_ids = {e.agent_id for e in events}
        assert "agent1" in agent_ids
        assert "agent2" in agent_ids

    @pytest.mark.asyncio
    async def test_empty_streams(self):
        """Test handling of empty stream dict."""
        multiplexer = StreamMultiplexer()

        events = []
        async for event in multiplexer.merge_streams({}):
            events.append(event)

        assert len(events) == 0

    @pytest.mark.asyncio
    async def test_agent_id_tagging(self):
        """Test that events are tagged with agent_id."""
        multiplexer = StreamMultiplexer()

        # Create stream without agent_id
        async def untagged_stream():
            yield StreamEvent(event_type=StreamEventType.TEXT_DELTA, data={"chunk": "test"})

        # Merge with agent_id
        streams = {"test-agent": untagged_stream()}
        events = []
        async for event in multiplexer.merge_streams(streams):
            events.append(event)

        # Verify agent_id added
        assert len(events) == 1
        assert events[0].agent_id == "test-agent"


@pytest.fixture
def mock_db():
    """Mock database session."""
    from unittest.mock import MagicMock

    return MagicMock()
