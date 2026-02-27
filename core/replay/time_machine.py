"""Time machine for conversation replay.

Provides time-travel capabilities to replay conversations at any point in time
using MatrixOne's snapshot feature.
"""

import re

from core.events.event_reader import EventReader
from core.events.models import ConversationEvent
from core.git_for_data import GitForData
from core.db_consumer import DbConsumer, DbFactory

_SAFE_NAME_RE = re.compile(r"^[a-zA-Z0-9_-]+$")

_DEFAULT_COLUMNS = [
    "event_id", "user_id", "session_id", "agent_id", "agent_version",
    "event_type", "content", "metadata", "created_at",
    "parent_event_id", "causal_chain_id",
]


def _validate_checkpoint_name(name: str) -> None:
    """Validate checkpoint name to prevent SQL injection.

    MatrixOne SNAPSHOT syntax does not support parameterized names,
    so we whitelist the character set.
    """
    if not _SAFE_NAME_RE.match(name):
        raise ValueError(
            f"Invalid checkpoint name: {name!r}. "
            "Only alphanumeric, dash, underscore allowed."
        )


class TimeMachine(DbConsumer):
    """Time machine for conversation replay via MatrixOne snapshots."""

    def __init__(self, db_factory: DbFactory) -> None:
        super().__init__(db_factory)
        self.git = GitForData(self._db_factory)
        self.reader = EventReader(self._db_factory)

    def create_checkpoint(self, checkpoint_name: str, description: str = "") -> dict:
        _validate_checkpoint_name(checkpoint_name)
        snapshot = self.git.create_snapshot(checkpoint_name)
        return {
            "checkpoint_name": checkpoint_name,
            "timestamp": snapshot.get("timestamp"),
            "description": description,
        }

    def restore_to_checkpoint(self, checkpoint_name: str) -> None:
        _validate_checkpoint_name(checkpoint_name)
        self.git.restore_from_snapshot(checkpoint_name)

    def get_events_at_checkpoint(
        self,
        checkpoint_name: str,
        session_id: str | None = None,
        limit: int = 100,
        columns: list[str] | None = None,
    ) -> list[ConversationEvent]:
        """Get events as they were at a checkpoint (read-only time-travel)."""
        _validate_checkpoint_name(checkpoint_name)

        if columns is None:
            columns = _DEFAULT_COLUMNS
        cols = ", ".join(columns)

        with self._db() as db:
            from sqlalchemy import text

            if session_id:
                query = text(f"""
                    SELECT {cols} FROM agent_events {{SNAPSHOT = '{checkpoint_name}'}}
                    WHERE session_id = :session_id
                    ORDER BY created_at DESC
                    LIMIT :lim
                """)
                rows = db.execute(query, {"session_id": session_id, "lim": limit}).fetchall()
            else:
                query = text(f"""
                    SELECT {cols} FROM agent_events {{SNAPSHOT = '{checkpoint_name}'}}
                    ORDER BY created_at DESC
                    LIMIT :lim
                """)
                rows = db.execute(query, {"lim": limit}).fetchall()

            return [self.reader._row_to_event(dict(row._mapping)) for row in rows]

    def list_checkpoints(self) -> list[dict]:
        return self.git.list_snapshots()

    def replay_conversation(self, session_id: str, checkpoint_name: str) -> dict:
        _validate_checkpoint_name(checkpoint_name)
        events = self.get_events_at_checkpoint(checkpoint_name, session_id)
        return {
            "session_id": session_id,
            "checkpoint_name": checkpoint_name,
            "event_count": len(events),
            "events": events,
            "first_event_at": events[0].created_at if events else None,
            "last_event_at": events[-1].created_at if events else None,
        }
