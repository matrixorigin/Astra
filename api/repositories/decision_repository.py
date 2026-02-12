"""Decision repository for ORM-based data access."""

from sqlalchemy.orm import Session as DBSession

from api.models import DecisionAudit as DecisionModel


class DecisionRepository:
    """Repository for decision audit operations."""
    
    def __init__(self, db: DBSession):
        self.db = db
    
    def create(self, decision_data: dict) -> DecisionModel:
        """Create decision record."""
        decision = DecisionModel(**decision_data)
        self.db.add(decision)
        self.db.commit()
        self.db.refresh(decision)
        return decision
    
    def get_by_id(self, decision_id: str) -> DecisionModel | None:
        """Get decision by ID."""
        return self.db.query(DecisionModel).filter(
            DecisionModel.decision_id == decision_id
        ).first()
    
    def get_by_id_with_user(self, decision_id: str, user_id: str) -> DecisionModel | None:
        """Get decision with user ownership check via session join."""
        from api.models import Session as SessionModel
        
        return self.db.query(DecisionModel).join(
            SessionModel,
            DecisionModel.session_id == SessionModel.session_id
        ).filter(
            DecisionModel.decision_id == decision_id,
            SessionModel.user_id == user_id
        ).first()
    
    def list_by_session(
        self,
        session_id: str,
        limit: int = 50,
        offset: int = 0
    ) -> tuple[list[DecisionModel], int]:
        """List decisions by session."""
        query = self.db.query(DecisionModel).filter(
            DecisionModel.session_id == session_id
        )
        
        total = query.count()
        
        query = query.order_by(DecisionModel.created_at.desc())
        query = query.offset(offset).limit(limit)
        
        return query.all(), total
    
    def list_by_user(
        self,
        user_id: str,
        decision_type: str | None = None,
        limit: int = 50,
        offset: int = 0
    ) -> tuple[list[DecisionModel], int]:
        """List decisions by user with optional type filter."""
        from api.models import Session as SessionModel
        
        query = self.db.query(DecisionModel).join(
            SessionModel,
            DecisionModel.session_id == SessionModel.session_id
        ).filter(
            SessionModel.user_id == user_id
        )
        
        if decision_type:
            query = query.filter(DecisionModel.decision_type == decision_type)
        
        total = query.count()
        
        query = query.order_by(DecisionModel.created_at.desc())
        query = query.offset(offset).limit(limit)
        
        return query.all(), total
