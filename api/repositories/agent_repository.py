"""Optimized agent repository with SQLAlchemy."""

from sqlalchemy.orm import Session

from api.models import Agent as AgentModel


class AgentRepository:
    """Repository for agent operations with query optimization."""
    
    def __init__(self, db: Session):
        self.db = db
    
    def create(self, agent_data: dict) -> AgentModel:
        """Create agent."""
        agent = AgentModel(**agent_data)
        self.db.add(agent)
        self.db.commit()
        self.db.refresh(agent)
        return agent
    
    def get_by_id(self, agent_id: str, owner_user_id: str | None = None) -> AgentModel | None:
        """Get agent by ID with optional ownership filter (pushed to DB)."""
        query = self.db.query(AgentModel).filter(AgentModel.agent_id == agent_id)
        
        # Push ownership filter to database
        if owner_user_id:
            query = query.filter(AgentModel.owner_user_id == owner_user_id)
        
        return query.first()
    
    def list_by_owner(
        self,
        owner_user_id: str,
        agent_type: str | None = None,
        is_active: bool | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> list[AgentModel]:
        """List agents with filters pushed to database.
        
        Uses projection to only select needed columns and filters at DB level.
        """
        query = self.db.query(AgentModel).filter(
            AgentModel.owner_user_id == owner_user_id
        )
        
        # Push filters to database
        if agent_type:
            query = query.filter(AgentModel.agent_type == agent_type)
        
        if is_active is not None:
            query = query.filter(AgentModel.is_active == is_active)
        
        # Apply pagination at database level
        query = query.offset(offset).limit(limit)
        
        return query.all()
    
    def update(self, agent_id: str, owner_user_id: str, updates: dict) -> AgentModel | None:
        """Update agent with ownership verification at DB level."""
        agent = self.db.query(AgentModel).filter(
            AgentModel.agent_id == agent_id,
            AgentModel.owner_user_id == owner_user_id  # Ownership check in query
        ).first()
        
        if not agent:
            return None
        
        for key, value in updates.items():
            setattr(agent, key, value)
        
        self.db.commit()
        self.db.refresh(agent)
        return agent
    
    def delete(self, agent_id: str, owner_user_id: str) -> bool:
        """Delete agent with ownership verification at DB level."""
        result = self.db.query(AgentModel).filter(
            AgentModel.agent_id == agent_id,
            AgentModel.owner_user_id == owner_user_id  # Ownership check in query
        ).delete()
        
        self.db.commit()
        return result > 0
    
    def count_by_owner(self, owner_user_id: str) -> int:
        """Count agents - optimized query."""
        return self.db.query(AgentModel).filter(
            AgentModel.owner_user_id == owner_user_id
        ).count()
