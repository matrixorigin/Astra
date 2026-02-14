"""Agent management module."""

import json
from datetime import datetime, timezone
from typing import Optional

from uuid_utils import uuid7

from core.logging_config import get_logger
from sqlalchemy.orm import Session
from api.database import get_db_session

logger = get_logger(__name__)


class AgentManager:
    """Manage agent operations."""

    def __init__(self, db: Session):
        """Initialize agent manager.

        Args:
            db: Session instance
        """
        self.db = db

    def create_agent(
        self,
        agent_name: str,
        owner_user_id: str,
        agent_type: str = "chatbot",
        config: Optional[dict] = None,
    ) -> dict:
        """Create a new agent.

        Args:
            agent_name: Agent name
            owner_user_id: Owner user ID
            agent_type: Agent type (chatbot, assistant, workflow, custom)
            config: Optional agent configuration

        Returns:
            Agent dictionary

        Raises:
            ValueError: If owner user does not exist
        """
        # Verify owner exists using ORM
        from api.models import User, Agent
        
        owner = self.db.query(User).filter(User.user_id == owner_user_id).first()
        
        if not owner:
            raise ValueError(f"User '{owner_user_id}' does not exist")

        agent_id = str(uuid7())
        
        new_agent = Agent(
            agent_id=agent_id,
            agent_name=agent_name,
            agent_type=agent_type,
            owner_user_id=owner_user_id,
            agent_config=config,  # ORM handles JSON serialization if configured, but model uses JSON type
            is_active=1,
            created_at=datetime.now(timezone.utc)
        )
        
        self.db.add(new_agent)
        self.db.commit()

        logger.info(f"Created agent: {agent_name} ({agent_id}) for user {owner_user_id}")

        return {
            "agent_id": agent_id,
            "agent_name": agent_name,
            "agent_type": agent_type,
            "owner_user_id": owner_user_id,
            "config": config,
        }

    def get_agent(self, agent_id: str) -> Optional[dict]:
        """Get agent by ID.

        Args:
            agent_id: Agent ID

        Returns:
            Agent dictionary if found, None otherwise
        """
        from api.models import Agent
        
        agent = self.db.query(Agent).filter(Agent.agent_id == agent_id).first()
        
        if not agent:
            return None
            
        return {
            "agent_id": agent.agent_id,
            "agent_name": agent.agent_name,
            "agent_type": agent.agent_type,
            "owner_user_id": agent.owner_user_id,
            "agent_config": agent.agent_config,
            "is_active": bool(agent.is_active),
            "created_at": agent.created_at
        }

    def list_agents(self, owner_user_id: Optional[str] = None) -> list[dict]:
        """List agents.

        Args:
            owner_user_id: Optional filter by owner user ID

        Returns:
            List of agent dictionaries
        """
        from api.models import Agent
        
        query = self.db.query(Agent)
        
        if owner_user_id:
            query = query.filter(Agent.owner_user_id == owner_user_id)
            
        agents = query.order_by(Agent.created_at.desc()).all()

        return [
            {
                "agent_id": agent.agent_id,
                "agent_name": agent.agent_name,
                "agent_type": agent.agent_type,
                "owner_user_id": agent.owner_user_id,
                "agent_config": agent.agent_config,
                "is_active": bool(agent.is_active),
                "created_at": agent.created_at
            }
            for agent in agents
        ]

    def update_agent(
        self,
        agent_id: str,
        agent_name: Optional[str] = None,
        config: Optional[dict] = None,
        is_active: Optional[bool] = None,
    ) -> dict:
        """Update agent.

        Args:
            agent_id: Agent ID
            agent_name: Optional new agent name
            config: Optional new configuration
            is_active: Optional active status

        Returns:
            Updated agent dictionary or empty dict if not found
        """
        from api.models import Agent
        
        agent = self.db.query(Agent).filter(Agent.agent_id == agent_id).first()
        
        if not agent:
            return {}

        if agent_name is not None:
            agent.agent_name = agent_name

        if config is not None:
            agent.agent_config = config

        if is_active is not None:
            agent.is_active = 1 if is_active else 0
            
        agent.updated_at = datetime.now(timezone.utc)
        self.db.commit()

        logger.info(f"Updated agent: {agent_id}")
        return self.get_agent(agent_id) or {}

    def delete_agent(self, agent_id: str) -> bool:
        """Delete agent.

        Args:
            agent_id: Agent ID

        Returns:
            True if agent was deleted, False if not found
        """
        from api.models import Agent
        
        agent = self.db.query(Agent).filter(Agent.agent_id == agent_id).first()
        
        if not agent:
            return False
            
        self.db.delete(agent)
        self.db.commit()
        logger.info(f"Deleted agent: {agent_id}")
        return True

    def verify_agent_owner(self, agent_id: str, user_id: str) -> bool:
        """Verify that user owns the agent.

        Args:
            agent_id: Agent ID
            user_id: User ID

        Returns:
            True if user owns the agent, False otherwise
        """
        from api.models import Agent
        
        agent = self.db.query(Agent).filter(Agent.agent_id == agent_id).first()

        if not agent:
            return False

        return agent.owner_user_id == user_id
