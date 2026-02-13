"""Time machine for conversation replay.

Provides time-travel capabilities to replay conversations at any point in time.
"""

from core.events.event_reader import EventReader
from core.events.models import ConversationEvent
from sqlalchemy.orm import Session
from api.database import get_db_session
from core.git_for_data import GitForData


class TimeMachine:
    """Time machine for conversation replay.

    Enables replaying conversations at specific points in time using
    MatrixOne's Git for Data capabilities.
    """

    def __init__(self, db: Session | None = None) -> None:
        """Initialize time machine.

        Args:
            db: SQLAlchemy Session instance. If None, creates a new one.
        """
        self.db = db or next(get_db_session())
        self.git = GitForData(db)
        self.reader = EventReader(db)

    def create_checkpoint(self, checkpoint_name: str, description: str = "") -> dict:
        """Create a checkpoint at the current time.

        Args:
            checkpoint_name: Name for the checkpoint
            description: Optional description

        Returns:
            dict: Checkpoint metadata
        """
        snapshot = self.git.create_snapshot(checkpoint_name)
        return {
            "checkpoint_name": checkpoint_name,
            "timestamp": snapshot.get("timestamp"),
            "description": description,
        }

    def restore_to_checkpoint(self, checkpoint_name: str) -> None:
        """Restore database state to a checkpoint.

        Args:
            checkpoint_name: Name of the checkpoint to restore

        Warning:
            This will restore the entire database state.
            All changes after the checkpoint will be lost.
        """
        self.git.restore_from_snapshot(checkpoint_name)

    def get_events_at_checkpoint(
        self,
        checkpoint_name: str,
        session_id: str | None = None,
        limit: int = 100,
        columns: list[str] | None = None,
    ) -> list[ConversationEvent]:
        """Get events as they were at a checkpoint.

        This uses MatrixOne's time-travel query feature (READ-ONLY).
        No database restore is performed, safe for production use.

        Args:
            checkpoint_name: Name of the checkpoint
            session_id: Optional session filter
            limit: Maximum number of events to return (default: 100)
            columns: Optional list of columns to select (default: all needed columns)

        Returns:
            list[ConversationEvent]: Events at that point in time

        Note:
            This is a read-only operation using {SNAPSHOT = 'name'} syntax.
            The current state is never affected.
        """
        # Select only needed columns to avoid large data transfer
        if columns is None:
            columns = [
                "event_id",
                "user_id",
                "session_id",
                "agent_id",
                "agent_version",
                "event_type",
                "content",
                "metadata",
                "created_at",
                "parent_event_id",
                "causal_chain_id",
            ]

        cols = ", ".join(columns)

        from sqlalchemy import text
        
        if session_id:
            query = text(f"""
                SELECT {cols} FROM conversation_events {{SNAPSHOT = '{checkpoint_name}'}}
                WHERE session_id = :session_id
                ORDER BY created_at DESC
                LIMIT {limit}
            """)
            rows = self.db.execute(query, {"session_id": session_id}).fetchall()
        else:
            query = text(f"""
                SELECT {cols} FROM conversation_events {{SNAPSHOT = '{checkpoint_name}'}}
                ORDER BY created_at DESC
                LIMIT {limit}
            """)
            rows = self.db.execute(query).fetchall()

        return [self.reader._row_to_event(dict(row._mapping)) for row in rows]

    def list_checkpoints(self) -> list[dict]:
        """List all available checkpoints.

        Returns:
            list[dict]: List of checkpoints with metadata
        """
        return self.git.list_snapshots()

    def replay_conversation(self, session_id: str, checkpoint_name: str) -> dict:
        """Replay a conversation as it was at a checkpoint.

        Args:
            session_id: Session to replay
            checkpoint_name: Checkpoint to replay from

        Returns:
            dict: Replay summary with events and metadata
        """
        events = self.get_events_at_checkpoint(checkpoint_name, session_id)

        return {
            "session_id": session_id,
            "checkpoint_name": checkpoint_name,
            "event_count": len(events),
            "events": events,
            "first_event_at": events[0].created_at if events else None,
            "last_event_at": events[-1].created_at if events else None,
        }
