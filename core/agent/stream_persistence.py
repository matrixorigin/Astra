"""Stream persistence layer for dual output (stream + log).

Every streamed chunk is simultaneously:
1. Delivered to the client in real-time
2. Logged to conversation_events for audit and replay
"""

import json
from collections.abc import AsyncIterator

from core.events.event_logger import EventLogger
from core.events.models import StreamEvent, StreamEventType
from core.logging_config import get_logger

logger = get_logger(__name__)


class StreamPersistence:
    """Handle dual output: stream to client + persist to database."""

    def __init__(self, event_logger: EventLogger):
        self.event_logger = event_logger

    async def persist_stream(
        self,
        stream: AsyncIterator[StreamEvent],
        user_id: str,
        session_id: str,
        agent_id: str,
        agent_version: str,
        causal_chain_id: str,
    ) -> AsyncIterator[StreamEvent]:
        """Wrap stream to persist each event while yielding.

        Args:
            stream: Original stream iterator
            user_id: User identifier
            session_id: Session identifier
            agent_id: Agent identifier
            agent_version: Agent version
            causal_chain_id: Causal chain ID for linking

        Yields:
            StreamEvent: Same events, but also logged to database
        """
        parent_event_id = None

        async for event in stream:
            # Yield to client immediately
            yield event

            # Log to database
            try:
                db_event = self._log_stream_event(
                    event=event,
                    user_id=user_id,
                    session_id=session_id,
                    agent_id=agent_id,
                    agent_version=agent_version,
                    parent_event_id=parent_event_id,
                    causal_chain_id=causal_chain_id,
                )
                # Chain events
                parent_event_id = db_event.event_id
            except Exception as e:
                logger.error(f"Failed to persist stream event: {e}")
                # Don't break stream on logging failure

    def _log_stream_event(
        self,
        event: StreamEvent,
        user_id: str,
        session_id: str,
        agent_id: str,
        agent_version: str,
        parent_event_id: str | None,
        causal_chain_id: str,
    ):
        """Log a single stream event to database.

        Args:
            event: StreamEvent to log
            user_id: User identifier
            session_id: Session identifier
            agent_id: Agent identifier
            agent_version: Agent version
            parent_event_id: Parent event in chain
            causal_chain_id: Causal chain ID

        Returns:
            ConversationEvent: Logged event
        """
        # Map StreamEventType to database event type
        event_type_str = self._map_stream_event_type(event.event_type)

        # Serialize event data
        content = json.dumps(
            {
                "event_type": event.event_type.value,
                "data": event.data,
                "stream_event_id": event.event_id,
            }
        )

        # Create metadata
        metadata = {
            "stream_event_type": event.event_type.value,
            "agent_id": event.agent_id or agent_id,
        }

        return self.event_logger.create_stream_event(
            user_id=user_id,
            session_id=session_id,
            event_type=event_type_str,
            content=content,
            agent_id=agent_id,
            agent_version=agent_version,
            parent_event_id=parent_event_id,
            causal_chain_id=causal_chain_id,
            metadata=metadata,
        )

    def _map_stream_event_type(self, stream_type: StreamEventType) -> str:
        """Map StreamEventType to database EventType string.

        Args:
            stream_type: StreamEventType enum value

        Returns:
            str: Database event type string
            
        Raises:
            ValueError: If stream_type is not mapped
        """
        # Map to existing EventType enum values
        mapping = {
            StreamEventType.RUN_STARTED: "stream_run_started",
            StreamEventType.RUN_FINISHED: "stream_run_finished",
            StreamEventType.RUN_ERROR: "stream_run_error",
            StreamEventType.TEXT_DELTA: "stream_text_delta",
            StreamEventType.TEXT_DONE: "stream_text_done",
            StreamEventType.THINKING_DELTA: "stream_thinking_delta",
            StreamEventType.THINKING_DONE: "stream_thinking_done",
            StreamEventType.TOOL_CALL_START: "stream_tool_call_start",
            StreamEventType.TOOL_CALL_ARGS: "stream_tool_call_args",
            StreamEventType.TOOL_CALL_END: "stream_tool_call_end",
            StreamEventType.TOOL_RESULT: "stream_tool_result",
            StreamEventType.PLAN_CREATED: "stream_plan_created",
            StreamEventType.PLAN_STEP_START: "stream_plan_step_start",
            StreamEventType.PLAN_STEP_DONE: "stream_plan_step_done",
            StreamEventType.PLAN_REVISED: "stream_plan_revised",
            StreamEventType.AGENT_DELEGATED: "stream_agent_delegated",
            StreamEventType.AGENT_PROGRESS: "stream_agent_progress",
            StreamEventType.AGENT_COMPLETED: "stream_agent_completed",
        }
        
        if stream_type not in mapping:
            raise ValueError(f"Unmapped StreamEventType: {stream_type}")
            
        return mapping[stream_type]
