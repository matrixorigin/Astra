"""Time machine for conversation replay.

Provides time-travel capabilities to replay conversations at any point in time.
"""

from datetime import datetime
from typing import Optional

from core.events.event_reader import EventReader
from core.events.models import ConversationEvent
from sdk.database import Database
from sdk.git_for_data import GitForData


class TimeMachine:
    """Time machine for conversation replay.
    
    Enables replaying conversations at specific points in time using
    MatrixOne's Git for Data capabilities.
    """

    def __init__(self, db: Optional[Database] = None) -> None:
        """Initialize time machine.
        
        Args:
            db: Database instance. If None, creates a new one.
        """
        self.db = db or Database()
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
        self, checkpoint_name: str, session_id: Optional[str] = None
    ) -> list[ConversationEvent]:
        """Get events as they were at a checkpoint.
        
        This creates a temporary restore to query historical state.
        
        Args:
            checkpoint_name: Name of the checkpoint
            session_id: Optional session filter
            
        Returns:
            list[ConversationEvent]: Events at that point in time
            
        Note:
            This is a read-only operation. The current state is preserved.
        """
        # Create a temporary snapshot of current state
        temp_snapshot = f"temp_current_{datetime.now().timestamp()}"
        self.git.create_snapshot(temp_snapshot)

        try:
            # Restore to checkpoint
            self.git.restore_from_snapshot(checkpoint_name)

            # Query events
            if session_id:
                events = self.reader.get_session_events(session_id)
            else:
                # Get recent events (limit to avoid large queries)
                query = """
                    SELECT * FROM conversation_events 
                    ORDER BY created_at DESC 
                    LIMIT 100
                """
                rows = self.db.fetchall(query)
                events = [self.reader._row_to_event(row) for row in rows]

            return events

        finally:
            # Restore to current state
            self.git.restore_from_snapshot(temp_snapshot)
            self.git.drop_snapshot(temp_snapshot)

    def list_checkpoints(self) -> list[dict]:
        """List all available checkpoints.
        
        Returns:
            list[dict]: List of checkpoints with metadata
        """
        return self.git.list_snapshots()

    def replay_conversation(
        self, session_id: str, checkpoint_name: str
    ) -> dict:
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
