"""Session manager for conversation lifecycle.

Handles session creation, updates, and lifecycle management.
"""

from datetime import datetime, timezone
from typing import Callable
from uuid import uuid4

from sqlalchemy.orm import Session as DBSession

from api.database import get_db_session
from api.models import Session as SessionModel
from core.events.session_models import Session, SessionStatus
from core.logging_config import get_logger

logger = get_logger(__name__)


class SessionManager:
    """Manager for conversation sessions.

    Provides methods to create, update, and manage session lifecycle.
    """

    def __init__(self, session: DBSession | None = None) -> None:
        """Initialize session manager.

        Args:
            session: SQLAlchemy session. If None, creates a new one.
        """
        self._session = session
        self._owns_session = session is None

    def _get_session(self) -> DBSession:
        """Get or create session."""
        if self._session is None:
            self._session = next(get_db_session())
        return self._session

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()

    def close(self):
        """Close session if we own it."""
        if self._owns_session and self._session:
            self._session.close()

    def __del__(self):
        """Close session if we own it."""
        self.close()

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
        
        db = self._get_session()
        db.add(db_session)
        db.commit()
        # Build return value before any lazy-load issues (expire_on_commit or detachment)
        result = Session(
            session_id=session_id,
            user_id=user_id,
            created_at=now,
            updated_at=now,
            last_active_at=now,
            status=SessionStatus.ACTIVE,
            event_count=0,
            metadata=metadata,
        )
        return result

    def get_session(self, session_id: str) -> Session | None:
        """Get a session by ID.

        Args:
            session_id: Session identifier

        Returns:
            Optional[Session]: Session if found, None otherwise
        """
        db = self._get_session()
        db_session = db.query(SessionModel).filter(SessionModel.session_id == session_id).first()
        return self._model_to_session(db_session) if db_session else None

    def update_session_activity(self, session_id: str, last_event_id: str) -> None:
        """Update session activity timestamp and last event.

        Args:
            session_id: Session identifier
            last_event_id: Last event identifier
        """
        db = self._get_session()
        now = datetime.now(timezone.utc)
        
        db_session = db.query(SessionModel).filter(SessionModel.session_id == session_id).first()
        if db_session:
            db_session.last_active_at = now
            db_session.last_event_id = last_event_id
            db_session.event_count += 1
            db_session.updated_at = now
            db.commit()

    def close_session(self, session_id: str, on_close: Callable[[str], None] | None = None) -> None:
        """Close a session and trigger knowledge extraction.

        Args:
            session_id: Session identifier
            on_close: Optional callback(session_id) for resource cleanup (e.g. sandbox)
        """
        db = self._get_session()
        db_session = db.query(SessionModel).filter(SessionModel.session_id == session_id).first()
        if db_session:
            db_session.status = SessionStatus.CLOSED
            db_session.updated_at = datetime.now(timezone.utc)
            db.commit()
            
            # Session-level quality scoring (aggregate chain scores)
            try:
                from core.evaluation.multi_level_scorer import score_session
                score_session(db, session_id)
            except Exception as e:
                logger.warning("Session-level scoring failed (non-fatal): %s", e)

            # Single query: fetch conversation events for both knowledge extraction and summary.
            # Avoids N+1: previous code queried distinct chain_ids then per-chain events,
            # plus a second scan for summary — now one query serves both.
            from api.models import Event
            events = db.query(Event).filter(
                Event.session_id == session_id,
                Event.event_type.in_(["user_query", "llm_response"]),
            ).order_by(Event.created_at).limit(200).all()

            # Auto-trigger knowledge extraction (batch by chain)
            try:
                from skills.knowledge.api import KnowledgeExtractor
                from core.events.event_logger import EventLogger

                event_logger = EventLogger.from_session(db)
                extractor = KnowledgeExtractor(db, event_logger=event_logger)

                chains: dict[str, list] = {}
                for e in events:
                    chains.setdefault(e.causal_chain_id, []).append(e)

                for chain_id, chain_events in chains.items():
                    extractor.extract_from_events(chain_id, db_session.user_id, chain_events)

                logger.info(f"Extracted knowledge from {len(chains)} chains in session {session_id}")
            except Exception as e:
                logger.error(f"Knowledge extraction failed for session {session_id}: {e}")

            # Resource cleanup (sandbox, etc.)
            if on_close:
                try:
                    on_close(session_id)
                except Exception as e:
                    logger.error(f"on_close callback failed for session {session_id}: {e}")

            # Generate full session summary (memories table) — reuse events already loaded
            try:
                from api.database import SessionLocal
                from core.memory import create_memory_service

                messages = [
                    {"role": "user" if e.event_type == "user_query" else "assistant",
                     "content": e.content or ""}
                    for e in events
                ]
                if messages:
                    svc = create_memory_service(SessionLocal, user_id=db_session.user_id)
                    svc.generate_session_summary(db_session.user_id, session_id, messages)
            except Exception as e:
                logger.debug("Session summary generation failed (non-fatal): %s", e)

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
        db = self._get_session()
        query = db.query(SessionModel).filter(SessionModel.user_id == user_id)
        
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
