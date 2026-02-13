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
        # Verify owner exists
        from sqlalchemy import text
        owner = self.db.execute(
            text("SELECT user_id FROM users WHERE user_id = :user_id"), 
            {"user_id": owner_user_id}
        ).fetchone()
        
        if not owner:
            raise ValueError(f"User '{owner_user_id}' does not exist")

        agent_id = str(uuid7())

        self.db.execute(
            text("""
            INSERT INTO agents (agent_id, agent_name, agent_type, owner_user_id, agent_config, is_active, created_at)
            VALUES (:agent_id, :agent_name, :agent_type, :owner_user_id, :agent_config, :is_active, :created_at)
            """),
            {
                "agent_id": agent_id,
                "agent_name": agent_name,
                "agent_type": agent_type,
                "owner_user_id": owner_user_id,
                "agent_config": None if config is None else json.dumps(config),
                "is_active": True,
                "created_at": datetime.now(timezone.utc),
            },
        )
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
        from sqlalchemy import text
        result = self.db.execute(
            text("""
            SELECT agent_id, agent_name, agent_type, owner_user_id, agent_config, is_active, created_at
            FROM agents WHERE agent_id = :agent_id
            """),
            {"agent_id": agent_id},
        )
        agent = result.mappings().first()

        return dict(agent) if agent else None

    def list_agents(self, owner_user_id: Optional[str] = None) -> list[dict]:
        """List agents.

        Args:
            owner_user_id: Optional filter by owner user ID

        Returns:
            List of agent dictionaries
        """
        from sqlalchemy import text
        
        if owner_user_id:
            result = self.db.execute(
                text("""
                SELECT agent_id, agent_name, agent_type, owner_user_id, agent_config, is_active, created_at
                FROM agents WHERE owner_user_id = :owner_user_id
                ORDER BY created_at DESC
                """),
                {"owner_user_id": owner_user_id}
            )
        else:
            result = self.db.execute(
                text("""
                SELECT agent_id, agent_name, agent_type, owner_user_id, agent_config, is_active, created_at
                FROM agents
                ORDER BY created_at DESC
                """)
            )

        return [dict(row._mapping) for row in result]

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
        from sqlalchemy import text
        
        updates = []
        params = {"agent_id": agent_id}

        if agent_name is not None:
            updates.append("agent_name = :agent_name")
            params["agent_name"] = agent_name

        if config is not None:
            updates.append("agent_config = :agent_config")
            params["agent_config"] = json.dumps(config)

        if is_active is not None:
            updates.append("is_active = :is_active")
            params["is_active"] = is_active

        if not updates:
            return {}

        updates.append("updated_at = :updated_at")
        params["updated_at"] = datetime.now(timezone.utc)

        query = f"UPDATE agents SET {', '.join(updates)} WHERE agent_id = :agent_id"
        result = self.db.execute(text(query), params)
        self.db.commit()

        if result.rowcount > 0:
            logger.info(f"Updated agent: {agent_id}")
            return self.get_agent(agent_id) or {}

        return {}

    def delete_agent(self, agent_id: str) -> bool:
        """Delete agent.

        Args:
            agent_id: Agent ID

        Returns:
            True if agent was deleted, False if not found
        """
        from sqlalchemy import text
        result = self.db.execute(
            text("DELETE FROM agents WHERE agent_id = :agent_id"),
            {"agent_id": agent_id}
        )
        self.db.commit()

        if result.rowcount > 0:
            logger.info(f"Deleted agent: {agent_id}")
            return True

        return False

    def verify_agent_owner(self, agent_id: str, user_id: str) -> bool:
        """Verify that user owns the agent.

        Args:
            agent_id: Agent ID
            user_id: User ID

        Returns:
            True if user owns the agent, False otherwise
        """
        from sqlalchemy import text
        result = self.db.execute(
            text("SELECT owner_user_id FROM agents WHERE agent_id = :agent_id"),
            {"agent_id": agent_id}
        )
        agent = result.first()

        if not agent:
            return False

        return agent.owner_user_id == user_id
