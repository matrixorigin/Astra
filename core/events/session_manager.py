"""Session manager for conversation lifecycle.

Handles session creation, updates, and lifecycle management.
"""

import json
from datetime import UTC, datetime
from typing import Optional
from uuid import uuid4

from core.events.session_models import Session, SessionStatus
from sdk.database import Database


class SessionManager:
    """Manager for conversation sessions.
    
    Provides methods to create, update, and manage session lifecycle.
    """

    def __init__(self, db: Optional[Database] = None) -> None:
        """Initialize session manager.
        
        Args:
            db: Database instance. If None, creates a new one.
        """
        self.db = db or Database()

    def create_session(
        self,
        user_id: str,
        tenant_id: Optional[str] = None,
        metadata: Optional[dict] = None,
    ) -> Session:
        """Create a new session.
        
        Args:
            user_id: User identifier
            tenant_id: Optional tenant identifier
            metadata: Optional metadata
            
        Returns:
            Session: Created session
        """
        session = Session(
            session_id=str(uuid4()),
            user_id=user_id,
            tenant_id=tenant_id,
            last_active_at=datetime.now(UTC),
            metadata=metadata,
        )

        query = """
            INSERT INTO sessions (
                session_id, user_id, tenant_id, created_at, updated_at,
                last_active_at, status, last_event_id, event_count,
                summary_status, summary_job_id, vector_db_snapshot_id, metadata
            ) VALUES (
                %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s
            )
        """

        params = (
            session.session_id,
            session.user_id,
            session.tenant_id,
            session.created_at,
            session.updated_at,
            session.last_active_at,
            session.status,
            session.last_event_id,
            session.event_count,
            session.summary_status,
            session.summary_job_id,
            session.vector_db_snapshot_id,
            json.dumps(session.metadata) if session.metadata else None,
        )

        self.db.execute(query, params)
        return session

    def get_session(self, session_id: str) -> Optional[Session]:
        """Get a session by ID.
        
        Args:
            session_id: Session identifier
            
        Returns:
            Optional[Session]: Session if found, None otherwise
        """
        query = "SELECT * FROM sessions WHERE session_id = %s"
        row = self.db.fetchone(query, (session_id,))
        return self._row_to_session(row) if row else None

    def update_session_activity(
        self, session_id: str, last_event_id: str
    ) -> None:
        """Update session activity timestamp and last event.
        
        Args:
            session_id: Session identifier
            last_event_id: Last event identifier
        """
        query = """
            UPDATE sessions 
            SET last_active_at = %s,
                last_event_id = %s,
                event_count = event_count + 1,
                updated_at = %s
            WHERE session_id = %s
        """
        now = datetime.now(UTC)
        self.db.execute(query, (now, last_event_id, now, session_id))

    def close_session(self, session_id: str) -> None:
        """Close a session.
        
        Args:
            session_id: Session identifier
        """
        query = """
            UPDATE sessions 
            SET status = %s, updated_at = %s
            WHERE session_id = %s
        """
        self.db.execute(query, (SessionStatus.CLOSED, datetime.now(UTC), session_id))

    def get_user_sessions(
        self, user_id: str, status: Optional[SessionStatus] = None, limit: int = 10
    ) -> list[Session]:
        """Get sessions for a user.
        
        Args:
            user_id: User identifier
            status: Optional status filter
            limit: Maximum number of sessions to return
            
        Returns:
            list[Session]: List of sessions
        """
        if status:
            query = """
                SELECT * FROM sessions 
                WHERE user_id = %s AND status = %s
                ORDER BY last_active_at DESC
                LIMIT %s
            """
            rows = self.db.fetchall(query, (user_id, status, limit))
        else:
            query = """
                SELECT * FROM sessions 
                WHERE user_id = %s
                ORDER BY last_active_at DESC
                LIMIT %s
            """
            rows = self.db.fetchall(query, (user_id, limit))

        return [self._row_to_session(row) for row in rows]

    def _row_to_session(self, row: dict) -> Session:
        """Convert database row to Session.
        
        Args:
            row: Database row as dictionary
            
        Returns:
            Session: Session object
        """
        metadata = json.loads(row["metadata"]) if row.get("metadata") else None

        return Session(
            session_id=row["session_id"],
            user_id=row["user_id"],
            tenant_id=row.get("tenant_id"),
            created_at=row["created_at"],
            updated_at=row["updated_at"],
            last_active_at=row.get("last_active_at"),
            status=row["status"],
            last_event_id=row.get("last_event_id"),
            event_count=row.get("event_count", 0),
            summary_status=row.get("summary_status"),
            summary_job_id=row.get("summary_job_id"),
            vector_db_snapshot_id=row.get("vector_db_snapshot_id"),
            metadata=metadata,
        )
