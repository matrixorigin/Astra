"""Stream replay engine for reconstructing streams from logged events.

Supports two replay paths:
1. Chunk-level: Read stream_text_delta from agent_run_events (high fidelity)
2. Full-text fallback: Read llm_response from agent_events

Chunk-level replay is gated on run completion to avoid partial reads.
Falls back to full-text when chunks are missing or run is incomplete.
"""

import json
import time
from collections.abc import AsyncIterator
from datetime import datetime

from sqlalchemy import and_, select, text
from sqlalchemy.orm import Session

from api.models import Event
from core.events.models import StreamEvent, StreamEventType
from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


class StreamReplay(DbConsumer):
    """Reconstruct streams from logged events."""

    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)

    async def replay_stream(
        self,
        session_id: str,
        causal_chain_id: str | None = None,
        run_id: str | None = None,
    ) -> AsyncIterator[StreamEvent]:
        """Replay stream from logged events.

        If run_id is provided, attempts chunk-level replay from agent_run_events first.
        Falls back to full-text replay from agent_events.

        Args:
            session_id: Session to replay
            causal_chain_id: Optional causal chain filter
            run_id: Optional run ID for chunk-level replay

        Yields:
            StreamEvent: Reconstructed stream events
        """
        # Try chunk-level replay from agent_run_events
        if run_id:
            chunks = self._load_run_chunks(run_id)
            if chunks is not None:
                # chunks is a list (possibly empty for tool-only runs)
                for chunk in chunks:
                    yield chunk
                return
            # chunks is None → run not complete or not found; fall back
            logger.warning(
                "No chunks in agent_run_events for run %s, falling back to full-text", run_id
            )
            for event in self._replay_fulltext_by_run(run_id):
                yield event
            return

        # Legacy path: stream_* events from agent_events
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
        """Get stream state at a specific point in time."""
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
                    "agent_id": event.event_metadata.get("agent_id")
                    if event.event_metadata
                    else None,
                }
            )

            event_type = event_data.get("event_type")
            if event_type == "run_started":
                state["status"] = "running"
            elif event_type == "run_finished":
                state["status"] = "finished"
            elif event_type == "run_error":
                state["status"] = "error"
            elif event_type in ("text_delta", "text_message_content"):
                state["text_accumulated"] += event_data.get("data", {}).get("delta", "")
            elif event_type in ("thinking_delta", "reasoning_message_content"):
                state["thinking_accumulated"] += event_data.get("data", {}).get("delta", "")
            elif event_type == "tool_call_start":
                state["active_tools"].append(event_data.get("data", {}).get("tool_name"))

        return state

    # ── Chunk-level replay (agent_run_events) ───────────────────────

    _TERMINAL_TYPES = (
        "run_completed",
        "run_failed",
        "run_cancelled",
        "stream_run_finished",
        "stream_run_error",
    )

    def _is_run_complete(self, run_id: str) -> bool:
        """Check if run has a terminal event in agent_run_events."""
        with self._db() as db:
            placeholders = ", ".join(f":t{i}" for i in range(len(self._TERMINAL_TYPES)))
            params = {f"t{i}": t for i, t in enumerate(self._TERMINAL_TYPES)}
            params["run_id"] = run_id
            query = text(
                f"SELECT 1 FROM agent_run_events "
                f"WHERE run_id = :run_id AND event_type IN ({placeholders}) LIMIT 1"
            )
            for attempt in range(4):
                row = db.execute(query, params).fetchone()
                if row is not None:
                    return True
                if attempt < 3:
                    time.sleep(0.05 * (attempt + 1))
            return False

    def _load_run_chunks(self, run_id: str) -> list[StreamEvent] | None:
        """Load chunk-level events from agent_run_events.

        Returns:
            list[StreamEvent]: Events (possibly empty) if run is complete.
            None: If run is not yet complete — caller should fall back.
        """
        with self._db() as db:
            if not self._is_run_complete(run_id):
                logger.info("Run %s not complete, skipping chunk replay", run_id)
                return None

            rows = db.execute(
                text(
                    "SELECT event_type, data, event_id, agent_id FROM agent_run_events "
                    "WHERE run_id = :run_id AND idx >= 0 ORDER BY idx"
                ),
                {"run_id": run_id},
            ).fetchall()
            if not rows:
                for attempt in range(3):
                    time.sleep(0.05 * (attempt + 1))
                    rows = db.execute(
                        text(
                            "SELECT event_type, data, event_id, agent_id FROM agent_run_events "
                            "WHERE run_id = :run_id AND idx >= 0 ORDER BY idx"
                        ),
                        {"run_id": run_id},
                    ).fetchall()
                    if rows:
                        break

            events = []
            for row in rows:
                data = row[1]
                if isinstance(data, str):
                    data = json.loads(data)

                try:
                    stream_type = StreamEventType(row[0])
                except ValueError:
                    # Not a recognized stream event type — include as raw
                    stream_type = StreamEventType.RAW

                events.append(
                    StreamEvent(
                        event_type=stream_type,
                        data=data,
                        event_id=row[2],
                        agent_id=row[3],
                    )
                )

            return events

    # ── Full-text fallback (agent_events) ──────────────

    def _replay_fulltext_by_run(self, run_id: str) -> list[StreamEvent]:
        """Replay from llm_response events in agent_events for a run.

        Synthesizes stream events from full-text responses when chunk-level
        data is unavailable (crash recovery, missing chunks).
        """
        with self._db() as db:
            rows = db.execute(
                text(
                    "SELECT event_id, content, agent_id, causal_chain_id "
                    "FROM agent_events "
                    "WHERE run_id = :run_id AND event_type = 'llm_response' "
                    "ORDER BY created_at"
                ),
                {"run_id": run_id},
            ).fetchall()

            if not rows:
                return []

            logger.info("Full-text replay for run %s: %d llm_response events", run_id, len(rows))

            events = []
            for row in rows:
                kwargs = {"event_id": row[0], "agent_id": row[2], "causal_chain_id": row[3]}
                events.append(
                    StreamEvent(
                        event_type=StreamEventType.TEXT_MESSAGE_START,
                        data={"role": "assistant"},
                        **kwargs,
                    )
                )
                events.append(
                    StreamEvent(
                        event_type=StreamEventType.TEXT_MESSAGE_CONTENT,
                        data={"delta": row[1] or ""},
                        **kwargs,
                    )
                )
                events.append(
                    StreamEvent(event_type=StreamEventType.TEXT_MESSAGE_END, data={}, **kwargs)
                )
            return events

    # ── Legacy path (stream_* in agent_events) ─────────

    def _query_stream_events(
        self,
        session_id: str,
        causal_chain_id: str | None = None,
        before_timestamp: datetime | None = None,
    ) -> list[Event]:
        """Query stream events from agent_events (legacy path)."""
        with self._db() as db:
            conditions = [
                Event.session_id == session_id,
                Event.event_type.in_(
                    [
                        "stream_run_started",
                        "stream_run_finished",
                        "stream_run_error",
                        "stream_step_started",
                        "stream_step_finished",
                        "stream_text_delta",
                        "stream_text_done",
                        "stream_text_message_start",
                        "stream_text_message_content",
                        "stream_text_message_end",
                        "stream_thinking_delta",
                        "stream_thinking_done",
                        "stream_reasoning_start",
                        "stream_reasoning_message_start",
                        "stream_reasoning_message_content",
                        "stream_reasoning_message_end",
                        "stream_reasoning_end",
                        "stream_tool_call_start",
                        "stream_tool_call_args",
                        "stream_tool_call_end",
                        "stream_tool_result",
                        "stream_state_snapshot",
                        "stream_state_delta",
                        "stream_messages_snapshot",
                        "stream_plan_created",
                        "stream_plan_step_start",
                        "stream_plan_step_done",
                        "stream_plan_revised",
                        "stream_agent_delegated",
                        "stream_agent_progress",
                        "stream_agent_completed",
                        "stream_custom",
                        "stream_raw",
                    ]
                ),
            ]

            if causal_chain_id:
                conditions.append(Event.causal_chain_id == causal_chain_id)
            if before_timestamp:
                conditions.append(Event.created_at <= before_timestamp)

            stmt = select(Event).where(and_(*conditions)).order_by(Event.created_at)

            result = db.execute(stmt)
            return list(result.scalars().all())

    def _reconstruct_stream_event(self, event: Event) -> StreamEvent | None:
        """Reconstruct StreamEvent from ConversationEvent."""
        try:
            event_data = self._parse_event_content(event)

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
                exc_info=True,
            )
            return None

    def _parse_event_content(self, event: Event) -> dict:
        """Parse event content JSON."""
        try:
            return json.loads(event.content)
        except json.JSONDecodeError:
            logger.error(f"Invalid JSON in event {event.event_id}")
            return {}
