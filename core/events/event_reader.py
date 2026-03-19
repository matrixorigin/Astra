"""Event reader for querying conversation events.

Provides methods to retrieve and query events from the database.
"""

import json
import time

from sqlalchemy import text
from sqlalchemy.engine import Connection, Engine
from sqlalchemy.orm import Query
from sqlalchemy.orm import sessionmaker

from api.models.agent import Event
from core.events.models import ContextSnapshot, ConversationEvent, TokenUsage
from core.db_consumer import DbConsumer, DbFactory

# Columns to load for list queries — excludes embedding (~6KB/row),
# context_snapshot, token_usage, skills_snapshot, llm_params (heavy JSON).
_LIST_COLUMNS = [
    Event.event_id,
    Event.user_id,
    Event.session_id,
    Event.agent_id,
    Event.agent_version,
    Event.event_type,
    Event.content,
    Event.event_metadata,
    Event.created_at,
    Event.parent_event_id,
    Event.causal_chain_id,
]


class EventReader(DbConsumer):
    """Reader for conversation events.

    Provides methods to query events by various criteria.
    """

    def __init__(self, db_factory: DbFactory) -> None:
        super().__init__(db_factory)

    def _row_to_event(self, row: dict) -> ConversationEvent:
        """Convert database row to ConversationEvent.

        Args:
            row: Database row as dictionary

        Returns:
            ConversationEvent: Event object
        """

        # Parse JSON fields — MatrixOne stores NULL as JSON string "null"
        def _parse_json(val):
            if not val or val == "null":
                return None
            return json.loads(val) if isinstance(val, str) else val

        def _parse_model(val, cls):
            if not val or val == "null":
                return None
            return cls.model_validate_json(val) if isinstance(val, str) else cls.model_validate(val)

        metadata = _parse_json(row.get("metadata"))
        context_snapshot = _parse_model(row.get("context_snapshot"), ContextSnapshot)
        token_usage = _parse_model(row.get("token_usage"), TokenUsage)
        skills_snapshot = _parse_json(row.get("skills_snapshot"))
        llm_params = _parse_json(row.get("llm_params"))

        return ConversationEvent(
            event_id=row["event_id"],
            user_id=row["user_id"],
            session_id=row["session_id"],
            agent_id=row["agent_id"],
            agent_version=row["agent_version"],
            event_type=row["event_type"],
            content=row["content"],
            desensitized_content=row.get("desensitized_content"),
            metadata=metadata,
            context_snapshot=context_snapshot,
            token_usage=token_usage,
            embedding_ref=row.get("embedding_ref"),
            created_at=row["created_at"],
            prompt_template_id=row.get("prompt_template_id"),
            skills_snapshot=skills_snapshot,
            quality_score=row.get("quality_score"),
            is_flagged=bool(row.get("is_flagged", False)),
            training_eligible=bool(row.get("training_eligible", False)),
            parent_event_id=row.get("parent_event_id"),
            causal_chain_id=row.get("causal_chain_id"),
            llm_model_used=row.get("llm_model_used"),
            llm_params=llm_params,
        )

    @staticmethod
    def _orm_row_to_dict(row) -> dict:
        """Convert ORM keyed-tuple row to dict with 'metadata' key."""
        d = row._asdict()
        # ORM column is 'event_metadata' but _row_to_event expects 'metadata'
        if "event_metadata" in d:
            d["metadata"] = d.pop("event_metadata")
        return d

    def get_event(self, event_id: str) -> ConversationEvent | None:
        """Get a single event by ID.

        Args:
            event_id: Event ID

        Returns:
            Optional[ConversationEvent]: Event if found, None otherwise
        """
        with self._db() as db:
            query = text("SELECT * FROM agent_events WHERE event_id = :event_id")
            result = db.execute(query, {"event_id": event_id})
            row = result.fetchone()

            if row:
                # Convert row (SQLAlchemy Row) to dict
                row_dict = dict(row._mapping)
                return self._row_to_event(row_dict)
            return None

    def get_session_events(
        self, session_id: str, limit: int | None = None
    ) -> list[ConversationEvent]:
        """Get all events for a session, ordered by creation time.

        Args:
            session_id: Session ID
            limit: Maximum number of events to return

        Returns:
            list[ConversationEvent]: List of events
        """
        with self._db() as db:
            rows = self._query_session_events(db, session_id, limit)
            bind = db.get_bind() if hasattr(db, "get_bind") else None
            if isinstance(bind, (Engine, Connection)):
                best_rows = rows
                for attempt in range(3):
                    fresh_db = sessionmaker(bind=bind, expire_on_commit=False)()
                    try:
                        fresh_rows = self._query_session_events(fresh_db, session_id, limit)
                    finally:
                        fresh_db.close()
                    if len(fresh_rows) > len(best_rows):
                        best_rows = fresh_rows
                    if attempt < 2:
                        time.sleep(0.03 * (attempt + 1))
                rows = best_rows
            return [self._row_to_event(self._orm_row_to_dict(r)) for r in rows]

    @staticmethod
    def _query_session_events(db, session_id: str, limit: int | None = None):
        q = (
            db.query(*_LIST_COLUMNS)
            .filter(
                Event.session_id == session_id,
            )
            .order_by(Event.created_at.asc())
        )
        if limit:
            q = q.limit(limit)
        return q.all()

    def get_user_events(self, user_id: str, limit: int | None = 100) -> list[ConversationEvent]:
        """Get all events for a user across sessions.

        Args:
            user_id: User ID
            limit: Maximum number of events to return (default: 100)

        Returns:
            list[ConversationEvent]: List of events
        """
        with self._db() as db:
            q = (
                db.query(*_LIST_COLUMNS)
                .filter(
                    Event.user_id == user_id,
                )
                .order_by(Event.created_at.desc())
            )
            if limit:
                q = q.limit(limit)
            rows = q.all()
            return [self._row_to_event(self._orm_row_to_dict(r)) for r in rows]

    def get_causal_chain(self, causal_chain_id: str) -> list[ConversationEvent]:
        """Get all events in a causal chain.

        Args:
            causal_chain_id: Causal chain ID

        Returns:
            list[ConversationEvent]: List of events in chronological order
        """
        with self._db() as db:
            rows = (
                db.query(*_LIST_COLUMNS)
                .filter(
                    Event.causal_chain_id == causal_chain_id,
                )
                .order_by(Event.created_at.asc())
                .all()
            )
            return [self._row_to_event(self._orm_row_to_dict(r)) for r in rows]
