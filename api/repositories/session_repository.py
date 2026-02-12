"""Optimized session repository."""

from sqlalchemy.orm import Session as DBSession

from api.models import Session as SessionModel


class SessionRepository:
    """Repository for session operations with query optimization."""
    
    def __init__(self, db: DBSession):
        self.db = db
    
    def create(self, session_data: dict) -> SessionModel:
        """Create session."""
        session = SessionModel(**session_data)
        self.db.add(session)
        self.db.commit()
        self.db.refresh(session)
        return session
    
    def get_by_id(self, session_id: str, user_id: str | None = None) -> SessionModel | None:
        """Get session with optional ownership filter pushed to DB."""
        query = self.db.query(SessionModel).filter(SessionModel.session_id == session_id)
        
        if user_id:
            query = query.filter(SessionModel.user_id == user_id)
        
        return query.first()
    
    def list_by_user(
        self,
        user_id: str,
        status: str | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> list[SessionModel]:
        """List sessions with filters pushed to database."""
        query = self.db.query(SessionModel).filter(SessionModel.user_id == user_id)
        
        # Push status filter to database
        if status:
            query = query.filter(SessionModel.status == status)
        
        # Order by most recent first
        query = query.order_by(SessionModel.last_active_at.desc())
        
        # Pagination at DB level
        query = query.offset(offset).limit(limit)
        
        return query.all()
    
    def update_status(self, session_id: str, user_id: str, status: str) -> SessionModel | None:
        """Update session status with ownership check at DB level."""
        session = self.db.query(SessionModel).filter(
            SessionModel.session_id == session_id,
            SessionModel.user_id == user_id
        ).first()
        
        if not session:
            return None
        
        session.status = status
        self.db.commit()
        self.db.refresh(session)
        return session
