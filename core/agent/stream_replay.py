"""Stream replay engine for reconstructing streams from logged events.

Enables:
- Complete stream replay from database
- Time-travel queries on stream state
- Stream forensics and debugging
"""

import json
from collections.abc import AsyncIterator
from datetime import datetime

from sqlalchemy import and_, select
from sqlalchemy.orm import Session

from api.models import Event
from core.events.models import StreamEvent, StreamEventType
from core.logging_config import get_logger

logger = get_logger(__name__)


class StreamReplay:
    """Reconstruct streams from logged events."""

    def __init__(self, db: Session):
        self.db = db

    async def replay_stream(
        self,
        session_id: str,
        causal_chain_id: str | None = None,
    ) -> AsyncIterator[StreamEvent]:
        """Replay stream from logged events.

        Args:
            session_id: Session to replay
            causal_chain_id: Optional causal chain filter

        Yields:
            StreamEvent: Reconstructed stream events
        """
        events = self._query_stream_events(session_id, causal_chain_id)

        for event in events:
            stream_event = self._reconstruct_stream_event(event)
            if stream_event:
                yield stream_event

    async def replay_stream_at(
        self,
        session_id: str,
        timestamp: datetime,
        causal_chain_id: str | None = None,
    ) -> AsyncIterator[StreamEvent]:
        """Replay stream up to a specific timestamp (time-travel).

        Args:
            session_id: Session to replay
            timestamp: Replay up to this point in time
            causal_chain_id: Optional causal chain filter

        Yields:
            StreamEvent: Reconstructed stream events up to timestamp
        """
        events = self._query_stream_events(session_id, causal_chain_id, timestamp)

        for event in events:
            stream_event = self._reconstruct_stream_event(event)
            if stream_event:
                yield stream_event

    def get_stream_state_at(
        self,
        session_id: str,
        timestamp: datetime,
        causal_chain_id: str | None = None,
    ) -> dict:
        """Get stream state at a specific point in time.

        Args:
            session_id: Session identifier
            timestamp: Point in time to query
            causal_chain_id: Optional causal chain filter

        Returns:
            dict: Stream state snapshot
        """
        events = self._query_stream_events(session_id, causal_chain_id, timestamp)

        state = {
            "session_id": session_id,
            "timestamp": timestamp.isoformat(),
            "causal_chain_id": causal_chain_id,
            "events": [],
            "active_tools": [],
            "text_accumulated": "",
            "thinking_accumulated": "",
            "status": "unknown",
        }

        for event in events:
            event_data = self._parse_event_content(event)
            state["events"].append(
                {
                    "event_id": event.event_id,
                    "event_type": event_data.get("event_type"),
                    "timestamp": event.created_at.isoformat(),
                    "agent_id": event.event_metadata.get("agent_id") if event.event_metadata else None,
                }
            )

            # Update state based on event type
            event_type = event_data.get("event_type")
            if event_type == "run_started":
                state["status"] = "running"
            elif event_type == "run_finished":
                state["status"] = "finished"
            elif event_type == "run_error":
                state["status"] = "error"
            elif event_type == "text_delta":
                state["text_accumulated"] += event_data.get("data", {}).get("delta", "")
            elif event_type == "thinking_delta":
                state["thinking_accumulated"] += event_data.get("data", {}).get("delta", "")
            elif event_type == "tool_call_start":
                state["active_tools"].append(event_data.get("data", {}).get("tool_name"))

        return state

    def _query_stream_events(
        self,
        session_id: str,
        causal_chain_id: str | None = None,
        before_timestamp: datetime | None = None,
    ) -> list[Event]:
        """Query stream events from database.

        Args:
            session_id: Session identifier
            causal_chain_id: Optional causal chain filter
            before_timestamp: Optional time limit

        Returns:
            list[Event]: Ordered stream events
        """
        # Build query
        conditions = [
            Event.session_id == session_id,
            Event.event_type.in_(
                [
                    "stream_run_started",
                    "stream_run_finished",
                    "stream_run_error",
                    "stream_text_delta",
                    "stream_text_done",
                    "stream_thinking_delta",
                    "stream_thinking_done",
                    "stream_tool_call_start",
                    "stream_tool_call_args",
                    "stream_tool_call_end",
                    "stream_tool_result",
                    "stream_plan_created",
                    "stream_plan_step_start",
                    "stream_plan_step_done",
                    "stream_plan_revised",
                    "stream_agent_delegated",
                    "stream_agent_progress",
                    "stream_agent_completed",
                ]
            ),
        ]

        if causal_chain_id:
            conditions.append(Event.causal_chain_id == causal_chain_id)

        if before_timestamp:
            conditions.append(Event.created_at <= before_timestamp)

        stmt = (
            select(Event)
            .where(and_(*conditions))
            .order_by(Event.created_at)
        )

        result = self.db.execute(stmt)
        return list(result.scalars().all())

    def _reconstruct_stream_event(self, event: Event) -> StreamEvent | None:
        """Reconstruct StreamEvent from ConversationEvent.

        Args:
            event: Database event

        Returns:
            StreamEvent: Reconstructed stream event, or None if invalid
            
        Note:
            Returns None for invalid events to allow partial replay.
            Logs errors for debugging but doesn't fail the entire stream.
        """
        try:
            event_data = self._parse_event_content(event)

            # Map event type
            stream_type_str = event_data.get("event_type")
            if not stream_type_str:
                logger.warning(f"No event_type in event {event.event_id}")
                return None

            try:
                stream_type = StreamEventType(stream_type_str)
            except ValueError:
                logger.warning(f"Invalid stream event type: {stream_type_str}")
                return None

            return StreamEvent(
                event_type=stream_type,
                data=event_data.get("data", {}),
                event_id=event_data.get("stream_event_id"),
                causal_chain_id=event.causal_chain_id,
                agent_id=event.event_metadata.get("agent_id") if event.event_metadata else None,
            )
        except Exception as e:
            logger.error(
                f"Failed to reconstruct stream event {event.event_id}: {e}",
                exc_info=True
            )
            return None

    def _parse_event_content(self, event: Event) -> dict:
        """Parse event content JSON.

        Args:
            event: Database event

        Returns:
            dict: Parsed content
        """
        try:
            return json.loads(event.content)
        except json.JSONDecodeError:
            logger.error(f"Invalid JSON in event {event.event_id}")
            return {}
