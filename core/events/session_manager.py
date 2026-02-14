"""Session manager for conversation lifecycle.

Handles session creation, updates, and lifecycle management.
"""

from datetime import datetime, timezone
from uuid import uuid4

from sqlalchemy.orm import Session as DBSession

from api.database import SessionLocal
from api.models import Session as SessionModel
from core.events.session_models import Session, SessionStatus


class SessionManager:
    """Manager for conversation sessions.

    Provides methods to create, update, and manage session lifecycle.
    """

    def __init__(self, session: DBSession | None = None) -> None:
        """Initialize session manager.

        Args:
            session: SQLAlchemy session. If None, creates a new one.
        """
        self._owns_session = session is None
        self.session = session or SessionLocal()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()

    def close(self):
        """Close session if we own it."""
        if self._owns_session:
            self.session.close()

    def create_session(
        self,
        user_id: str,
        metadata: dict | None = None,
    ) -> Session:
        """Create a new session.

        Args:
            user_id: User identifier
            metadata: Optional metadata

        Returns:
            Session: Created session
        """
        session_id = str(uuid4())
        now = datetime.now(timezone.utc)
        
        db_session = SessionModel(
            session_id=session_id,
            user_id=user_id,
            created_at=now,
            updated_at=now,
            last_active_at=now,
            status=SessionStatus.ACTIVE,
            event_count=0,
            session_metadata=metadata,
        )
        
        self.session.add(db_session)
        self.session.commit()
        self.session.refresh(db_session)
        
        return self._model_to_session(db_session)

    def get_session(self, session_id: str) -> Session | None:
        """Get a session by ID.

        Args:
            session_id: Session identifier

        Returns:
            Optional[Session]: Session if found, None otherwise
        """
        db_session = self.session.query(SessionModel).filter(SessionModel.session_id == session_id).first()
        return self._model_to_session(db_session) if db_session else None

    def update_session_activity(self, session_id: str, last_event_id: str) -> None:
        """Update session activity timestamp and last event.

        Args:
            session_id: Session identifier
            last_event_id: Last event identifier
        """
        now = datetime.now(timezone.utc)
        
        db_session = self.session.query(SessionModel).filter(SessionModel.session_id == session_id).first()
        if db_session:
            db_session.last_active_at = now
            db_session.last_event_id = last_event_id
            db_session.event_count += 1
            db_session.updated_at = now
            self.session.commit()

    def close_session(self, session_id: str) -> None:
        """Close a session.

        Args:
            session_id: Session identifier
        """
        db_session = self.session.query(SessionModel).filter(SessionModel.session_id == session_id).first()
        if db_session:
            db_session.status = SessionStatus.CLOSED
            db_session.updated_at = datetime.now(timezone.utc)
            self.session.commit()

    def get_user_sessions(
        self, user_id: str, status: SessionStatus | None = None, limit: int = 10
    ) -> list[Session]:
        """Get sessions for a user.

        Args:
            user_id: User identifier
            status: Optional status filter
            limit: Maximum number of sessions to return

        Returns:
            list[Session]: List of sessions
        """
        query = self.session.query(SessionModel).filter(SessionModel.user_id == user_id)
        
        if status:
            query = query.filter(SessionModel.status == status)
        
        query = query.order_by(SessionModel.last_active_at.desc()).limit(limit)
        db_sessions = query.all()
        
        return [self._model_to_session(s) for s in db_sessions]

    def _model_to_session(self, model: SessionModel) -> Session:
        """Convert SQLAlchemy model to Pydantic Session.

        Args:
            model: SQLAlchemy Session model

        Returns:
            Session: Pydantic Session object
        """
        return Session(
            session_id=model.session_id,
            user_id=model.user_id,
            created_at=model.created_at,
            updated_at=model.updated_at,
            last_active_at=model.last_active_at,
            status=model.status,
            last_event_id=model.last_event_id,
            event_count=model.event_count,
            summary_status=model.summary_status,
            summary_job_id=model.summary_job_id,
            vector_db_snapshot_id=model.vector_db_snapshot_id,
            metadata=model.session_metadata,
        )
