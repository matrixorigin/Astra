"""Optimized event repository."""

from collections.abc import Callable

from sqlalchemy.orm import Session as DBSession

from api.models import Event as EventModel


class EventRepository:
    """Repository for event operations with query optimization."""

    def __init__(self, db_factory: Callable[[], DBSession]):
        self._db_factory = db_factory

    @property
    def db(self) -> DBSession:
        return self._db_factory()

    def create(self, event_data: dict) -> EventModel:
        """Create event."""
        db = self.db
        event = EventModel(**event_data)
        db.add(event)
        db.commit()
        db.refresh(event)
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
        query = self.db.query(EventModel).filter(
            EventModel.session_id == session_id,
            EventModel.user_id == user_id
        )
        if event_type:
            query = query.filter(EventModel.event_type == event_type)
        return query.order_by(EventModel.created_at.asc()).offset(offset).limit(limit).all()

    def count_by_session(self, session_id: str) -> int:
        """Count events for session."""
        return self.db.query(EventModel).filter(
            EventModel.session_id == session_id
        ).count()

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
        return self.db.query(EventModel).filter(
            EventModel.causal_chain_id == causal_chain_id,
            EventModel.user_id == user_id
        ).order_by(EventModel.created_at.asc()).all()

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
