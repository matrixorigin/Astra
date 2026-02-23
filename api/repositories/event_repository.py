"""Optimized event repository."""

from sqlalchemy.orm import Session as DBSession

from api.models import Event as EventModel


class EventRepository:
    """Repository for event operations with query optimization."""

    def __init__(self, db: DBSession):
        self.db = db

    def create(self, event_data: dict) -> EventModel:
        """Create event."""
        event = EventModel(**event_data)
        self.db.add(event)
        self.db.commit()
        self.db.refresh(event)
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
        """List events with filters and pagination pushed to database.
        
        All filters are applied at database level for optimal performance.
        """
        query = self.db.query(EventModel).filter(
            EventModel.session_id == session_id,
            EventModel.user_id == user_id  # Ownership check in query
        )

        # Push event_type filter to database
        if event_type:
            query = query.filter(EventModel.event_type == event_type)

        # Order by creation time
        query = query.order_by(EventModel.created_at.asc())

        # Pagination at DB level
        query = query.offset(offset).limit(limit)

        return query.all()

    def count_by_session(self, session_id: str) -> int:
        """Count events for session - optimized."""
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

        # Get total count
        total = query.count()

        # Order by creation time desc
        query = query.order_by(EventModel.created_at.desc())

        # Pagination
        query = query.offset(offset).limit(limit)

        return query.all(), total

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

        query = query.order_by(EventModel.created_at.asc())
        query = query.offset(offset).limit(limit)

        return query.all(), total

    def delete(self, event_id: str) -> bool:
        """Delete event."""
        event = self.db.query(EventModel).filter(EventModel.event_id == event_id).first()

        if not event:
            return False

        self.db.delete(event)
        self.db.commit()
        return True
