"""Optimized agent repository with SQLAlchemy."""

from collections.abc import Callable

from sqlalchemy.orm import Session

from api.models import Agent as AgentModel


class AgentRepository:
    """Repository for agent operations with query optimization.

    Accepts a ``db_factory`` that returns the *current* request-scoped session.
    Within a single HTTP request the factory always returns the same ``Session``
    instance, so accessing ``self.db`` multiple times is safe.  Methods that
    need add/commit/refresh still capture the session in a local variable to
    make the single-session assumption explicit and easy to audit.
    """

    def __init__(self, db_factory: Callable[[], Session]):
        self._db_factory = db_factory

    @property
    def db(self) -> Session:
        return self._db_factory()

    def create(self, agent_data: dict) -> AgentModel:
        """Create agent."""
        db = self.db
        agent = AgentModel(**agent_data)
        db.add(agent)
        db.commit()
        return db.query(AgentModel).filter(AgentModel.agent_id == agent.agent_id).first() or agent

    def get_by_id(self, agent_id: str, owner_user_id: str | None = None) -> AgentModel | None:
        """Get agent by ID with optional ownership filter (pushed to DB)."""
        query = self.db.query(AgentModel).filter(AgentModel.agent_id == agent_id)
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
        """List agents with filters pushed to database."""
        query = self.db.query(AgentModel).filter(AgentModel.owner_user_id == owner_user_id)
        if agent_type:
            query = query.filter(AgentModel.agent_type == agent_type)
        if is_active is not None:
            query = query.filter(AgentModel.is_active == is_active)
        return query.offset(offset).limit(limit).all()

    def update(self, agent_id: str, owner_user_id: str, updates: dict) -> AgentModel | None:
        """Update agent with ownership verification at DB level."""
        db = self.db
        agent = (
            db.query(AgentModel)
            .filter(AgentModel.agent_id == agent_id, AgentModel.owner_user_id == owner_user_id)
            .first()
        )
        if not agent:
            return None
        for key, value in updates.items():
            setattr(agent, key, value)
        db.commit()
        return db.query(AgentModel).filter(AgentModel.agent_id == agent.agent_id).first() or agent

    def delete(self, agent_id: str, owner_user_id: str) -> bool:
        """Delete agent with ownership verification at DB level."""
        db = self.db
        result = (
            db.query(AgentModel)
            .filter(AgentModel.agent_id == agent_id, AgentModel.owner_user_id == owner_user_id)
            .delete()
        )
        db.commit()
        return result > 0

    def count_by_owner(self, owner_user_id: str) -> int:
        """Count agents - optimized query."""
        return self.db.query(AgentModel).filter(AgentModel.owner_user_id == owner_user_id).count()
