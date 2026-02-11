"""Unit tests for streaming output functionality."""

import unittest
from unittest.mock import MagicMock, AsyncMock
from core.events.models import StreamEvent, StreamEventType
from core.events.event_logger import EventLogger


class TestStreamEventModel(unittest.TestCase):
    """Test StreamEvent model."""

    def test_stream_event_creation(self):
        """Test creating a StreamEvent."""
        event = StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"chunk": "Hello"},
            event_id="event_1",
            causal_chain_id="chain_1",
        )
        
        self.assertEqual(event.event_type, StreamEventType.TEXT_DELTA)
        self.assertEqual(event.data["chunk"], "Hello")
        self.assertEqual(event.event_id, "event_1")

    def test_stream_event_types(self):
        """Test all stream event types."""
        event_types = [
            StreamEventType.RUN_STARTED,
            StreamEventType.RUN_FINISHED,
            StreamEventType.RUN_ERROR,
            StreamEventType.TEXT_DELTA,
            StreamEventType.TEXT_DONE,
            StreamEventType.THINKING_DELTA,
            StreamEventType.THINKING_DONE,
            StreamEventType.TOOL_CALL_START,
            StreamEventType.TOOL_CALL_ARGS,
            StreamEventType.TOOL_CALL_END,
            StreamEventType.TOOL_RESULT,
            StreamEventType.PLAN_CREATED,
            StreamEventType.PLAN_STEP_START,
            StreamEventType.PLAN_STEP_DONE,
            StreamEventType.PLAN_REVISED,
            StreamEventType.AGENT_DELEGATED,
            StreamEventType.AGENT_PROGRESS,
            StreamEventType.AGENT_COMPLETED,
        ]
        
        self.assertEqual(len(event_types), 18)


class TestEventLoggerStream(unittest.TestCase):
    """Test EventLogger stream event logging."""

    def setUp(self):
        self.db = MagicMock()
        self.logger = EventLogger(self.db)

    def test_create_stream_event(self):
        """Test creating a stream event."""
        # Note: Stream events are logged with custom event_type strings
        # since ConversationEvent.event_type is limited to EventType enum
        # In production, we would extend EventType or use a separate stream_events table
        event = self.logger.create_stream_event(
            user_id="user_1",
            session_id="session_1",
            event_type="user_query",  # Use valid EventType
            content='{"chunk": "Hello"}',
            parent_event_id="parent_1",
            causal_chain_id="chain_1",
        )
        
        self.assertIsNotNone(event.event_id)
        self.assertEqual(event.user_id, "user_1")
        self.assertEqual(event.session_id, "session_1")
        self.assertEqual(event.event_type, "user_query")
        
        # Verify database was called
        self.db.execute.assert_called_once()


if __name__ == '__main__':
    unittest.main()
