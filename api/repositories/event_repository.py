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
