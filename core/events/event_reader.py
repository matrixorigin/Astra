"""Event reader for querying conversation events.

Provides methods to retrieve and query events from the database.
"""

import json

from core.events.models import ContextSnapshot, ConversationEvent, TokenUsage
from sqlalchemy.orm import Session
from api.database import get_db_session


class EventReader:
    """Reader for conversation events.

    Provides methods to query events by various criteria.
    """

    def __init__(self, db: Session | None = None) -> None:
        """Initialize event reader.

        Args:
            db: SQLAlchemy Session instance. If None, creates a new one.
        """
        self.db = db or next(get_db_session())

    def _row_to_event(self, row: dict) -> ConversationEvent:
        """Convert database row to ConversationEvent.

        Args:
            row: Database row as dictionary

        Returns:
            ConversationEvent: Event object
        """
        # Parse JSON fields
        metadata = json.loads(row["metadata"]) if row.get("metadata") else None
        context_snapshot = (
            ContextSnapshot.model_validate_json(row["context_snapshot"])
            if row.get("context_snapshot")
            else None
        )
        token_usage = (
            TokenUsage.model_validate_json(row["token_usage"]) if row.get("token_usage") else None
        )
        skills_snapshot = json.loads(row["skills_snapshot"]) if row.get("skills_snapshot") else None
        llm_params = json.loads(row["llm_params"]) if row.get("llm_params") else None

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

    def get_event(self, event_id: str) -> ConversationEvent | None:
        """Get a single event by ID.

        Args:
            event_id: Event ID

        Returns:
            Optional[ConversationEvent]: Event if found, None otherwise
        """
        query = "SELECT * FROM conversation_events WHERE event_id = %s"
        row = self.db.fetchone(query, (event_id,))
        return self._row_to_event(row) if row else None

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
        # Select only needed columns
        columns = """
            event_id, user_id, session_id, agent_id, agent_version,
            event_type, content, metadata, created_at,
            parent_event_id, causal_chain_id
        """

        query = f"""
            SELECT {columns} FROM conversation_events
            WHERE session_id = %s
            ORDER BY created_at DESC
        """
        if limit:
            query += f" LIMIT {limit}"

        rows = self.db.fetchall(query, (session_id,))
        return [self._row_to_event(row) for row in rows]

    def get_user_events(self, user_id: str, limit: int | None = 100) -> list[ConversationEvent]:
        """Get all events for a user across sessions.

        Args:
            user_id: User ID
            limit: Maximum number of events to return (default: 100)

        Returns:
            list[ConversationEvent]: List of events
        """
        columns = """
            event_id, user_id, session_id, agent_id, agent_version,
            event_type, content, metadata, created_at,
            parent_event_id, causal_chain_id
        """

        query = f"""
            SELECT {columns} FROM conversation_events
            WHERE user_id = %s
            ORDER BY created_at DESC
        """
        if limit:
            query += f" LIMIT {limit}"

        rows = self.db.fetchall(query, (user_id,))
        return [self._row_to_event(row) for row in rows]

    def get_causal_chain(self, causal_chain_id: str) -> list[ConversationEvent]:
        """Get all events in a causal chain.

        Args:
            causal_chain_id: Causal chain ID

        Returns:
            list[ConversationEvent]: List of events in chronological order
        """
        query = """
            SELECT * FROM conversation_events
            WHERE causal_chain_id = %s
            ORDER BY created_at ASC
        """
        rows = self.db.fetchall(query, (causal_chain_id,))
        return [self._row_to_event(row) for row in rows]
