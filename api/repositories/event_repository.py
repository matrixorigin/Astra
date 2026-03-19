"""Optimized event repository."""

from collections.abc import Callable
import time

from sqlalchemy.engine import Connection, Engine
from sqlalchemy.orm import Session as DBSession
from sqlalchemy.orm import sessionmaker

from api.models import Event as EventModel


class EventRepository:
    """Repository for event operations with query optimization."""

    def __init__(self, db_factory: Callable[[], DBSession]):
        self._db_factory = db_factory

    @property
    def db(self) -> DBSession:
        return self._db_factory()

    @staticmethod
    def _query(db: DBSession):
        return db.query(EventModel).populate_existing()

    def create(self, event_data: dict) -> EventModel:
        """Create event."""
        db = self.db
        event = EventModel(**event_data)
        db.add(event)
        db.flush()
        db.commit()
        row = self._query(db).filter(EventModel.event_id == event.event_id).first()
        if row is not None:
            return row
        bind = db.get_bind()
        if isinstance(bind, (Engine, Connection)):
            fresh_db = sessionmaker(bind=bind, expire_on_commit=False)()
            try:
                return self._query(fresh_db).filter(EventModel.event_id == event.event_id).first() or event
            finally:
                fresh_db.close()
        return event

    def get_by_id(self, event_id: str, user_id: str | None = None) -> EventModel | None:
        """Get event with optional ownership filter."""
        query = self.db.query(EventModel).filter(EventModel.event_id == event_id)
        if user_id:
            query = query.filter(EventModel.user_id == user_id)
        return query.first()

    def list_by_session(
        self,
        session_id: str,
        user_id: str,
        event_type: str | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> list[EventModel]:
        """List events with filters and pagination pushed to database."""
        db = self.db
        query = self._query(db).filter(EventModel.session_id == session_id, EventModel.user_id == user_id)
        if event_type:
            query = query.filter(EventModel.event_type == event_type)
        rows = query.order_by(EventModel.created_at.asc()).offset(offset).limit(limit).all()

        bind = db.get_bind()
        if isinstance(bind, (Engine, Connection)):
            best_rows = rows
            for attempt in range(3):
                fresh_db = sessionmaker(bind=bind, expire_on_commit=False)()
                try:
                    fresh_query = self._query(fresh_db).filter(
                        EventModel.session_id == session_id,
                        EventModel.user_id == user_id,
                    )
                    if event_type:
                        fresh_query = fresh_query.filter(EventModel.event_type == event_type)
                    fresh_rows = (
                        fresh_query.order_by(EventModel.created_at.asc())
                        .offset(offset)
                        .limit(limit)
                        .all()
                    )
                finally:
                    fresh_db.close()
                if len(fresh_rows) > len(best_rows):
                    best_rows = fresh_rows
                if attempt < 2:
                    time.sleep(0.03 * (attempt + 1))
            rows = best_rows
        return rows

    def count_by_session(self, session_id: str) -> int:
        """Count events for session."""
        return self.db.query(EventModel).filter(EventModel.session_id == session_id).count()

    def get_by_user(
        self,
        user_id: str,
        session_id: str | None = None,
        event_type: str | None = None,
        agent_id: str | None = None,
        causal_chain_id: str | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> tuple[list[EventModel], int]:
        """List events by user with filters."""
        query = self.db.query(EventModel).filter(EventModel.user_id == user_id)
        if session_id:
            query = query.filter(EventModel.session_id == session_id)
        if event_type:
            query = query.filter(EventModel.event_type == event_type)
        if agent_id:
            query = query.filter(EventModel.agent_id == agent_id)
        if causal_chain_id:
            query = query.filter(EventModel.causal_chain_id == causal_chain_id)
        total = query.count()
        return query.order_by(EventModel.created_at.desc()).offset(offset).limit(limit).all(), total

    def get_by_causal_chain(self, causal_chain_id: str, user_id: str) -> list[EventModel]:
        """Get events by causal chain."""
        return (
            self.db.query(EventModel)
            .filter(EventModel.causal_chain_id == causal_chain_id, EventModel.user_id == user_id)
            .order_by(EventModel.created_at.asc())
            .all()
        )

    def get_by_session(
        self,
        session_id: str,
        limit: int = 100,
        offset: int = 0,
    ) -> tuple[list[EventModel], int]:
        """Get events by session."""
        query = self.db.query(EventModel).filter(EventModel.session_id == session_id)
        total = query.count()
        return query.order_by(EventModel.created_at.asc()).offset(offset).limit(limit).all(), total

    def delete(self, event_id: str) -> bool:
        """Delete event."""
        db = self.db
        event = db.query(EventModel).filter(EventModel.event_id == event_id).first()
        if not event:
            return False
        db.delete(event)
        db.commit()
        return True
