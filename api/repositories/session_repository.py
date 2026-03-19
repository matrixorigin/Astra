"""Optimized session repository."""

from collections.abc import Callable
import time

from sqlalchemy.engine import Connection, Engine
from sqlalchemy.orm import Session as DBSession
from sqlalchemy.orm import sessionmaker

from api.models import Session as SessionModel


class SessionRepository:
    """Repository for session operations with query optimization."""

    def __init__(self, db_factory: Callable[[], DBSession]):
        self._db_factory = db_factory

    @property
    def db(self) -> DBSession:
        return self._db_factory()

    @staticmethod
    def _query(db: DBSession):
        """Use populate_existing so request-scoped sessions don't reuse stale rows."""
        return db.query(SessionModel).populate_existing()

    def create(self, session_data: dict) -> SessionModel:
        """Create session."""
        db = self.db
        session = SessionModel(**session_data)
        db.add(session)
        db.flush()
        db.commit()
        row = self._query(db).filter(SessionModel.session_id == session.session_id).first()
        if row is not None:
            return row

        bind = db.get_bind()
        if isinstance(bind, (Engine, Connection)):
            fresh_factory = sessionmaker(bind=bind, expire_on_commit=False)
            for attempt in range(6):
                fresh_db = fresh_factory()
                try:
                    visible = (
                        self._query(fresh_db).filter(SessionModel.session_id == session.session_id).first()
                    )
                finally:
                    fresh_db.close()
                if visible is not None:
                    db.expire_all()
                    row = self._query(db).filter(SessionModel.session_id == session.session_id).first()
                    if row is not None:
                        return row
                if attempt < 5:
                    time.sleep(0.03 * (attempt + 1))
        return session

    def get_by_id(self, session_id: str, user_id: str | None = None) -> SessionModel | None:
        """Get session with optional ownership filter pushed to DB."""
        db = self.db
        db.expire_all()
        query = self._query(db).filter(SessionModel.session_id == session_id)
        if user_id:
            query = query.filter(SessionModel.user_id == user_id)
        return query.first()

    def list_by_user(
        self,
        user_id: str,
        agent_id: str | None = None,
        status: str | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> tuple[list[SessionModel], int]:
        """List sessions with filters pushed to database."""
        db = self.db
        db.expire_all()
        query = self._query(db).filter(SessionModel.user_id == user_id)
        if agent_id:
            query = query.filter(SessionModel.agent_id == agent_id)
        if status:
            query = query.filter(SessionModel.status == status)
        total = query.count()
        return query.order_by(SessionModel.created_at.desc()).offset(offset).limit(
            limit
        ).all(), total

    def update_status(self, session_id: str, user_id: str, status: str) -> SessionModel | None:
        """Update session status with ownership check at DB level."""
        db = self.db
        session = (
            self._query(db)
            .filter(SessionModel.session_id == session_id, SessionModel.user_id == user_id)
            .first()
        )
        if not session:
            return None
        session.status = status
        db.commit()
        db.expire_all()
        return (
            self._query(db).filter(SessionModel.session_id == session.session_id).first() or session
        )

    def update(self, session_id: str, update_data: dict) -> SessionModel | None:
        """Update session with data."""
        db = self.db
        session = self._query(db).filter(SessionModel.session_id == session_id).first()
        if not session:
            return None
        for key, value in update_data.items():
            setattr(session, key, value)
        db.commit()
        db.expire_all()
        return (
            self._query(db).filter(SessionModel.session_id == session.session_id).first() or session
        )

    def delete(self, session_id: str) -> bool:
        """Delete session."""
        db = self.db
        session = self._query(db).filter(SessionModel.session_id == session_id).first()
        if not session:
            return False
        db.delete(session)
        db.commit()
        db.expire_all()
        return True
