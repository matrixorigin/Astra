"""Agent management module."""

from datetime import datetime, timezone
from typing import Optional

from uuid_utils import uuid7

from core.logging_config import get_logger
from db.database import Database

logger = get_logger(__name__)


class AgentManager:
    """Manage agent operations."""

    def __init__(self, db: Database):
        """Initialize agent manager.

        Args:
            db: Database instance
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
        owner = self.db.fetchone(
            "SELECT user_id FROM users WHERE user_id = %s", (owner_user_id,)
        )
        if not owner:
            raise ValueError(f"User '{owner_user_id}' does not exist")

        agent_id = str(uuid7())

        self.db.execute(
            """
            INSERT INTO agents (agent_id, agent_name, agent_type, owner_user_id, config, created_at)
            VALUES (%s, %s, %s, %s, %s, %s)
            """,
            (
                agent_id,
                agent_name,
                agent_type,
                owner_user_id,
                None if config is None else str(config),
                datetime.now(timezone.utc),
            ),
        )

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
        agent = self.db.fetchone(
            """
            SELECT agent_id, agent_name, agent_type, owner_user_id, config, is_active, created_at
            FROM agents WHERE agent_id = %s
            """,
            (agent_id,),
        )

        return agent

    def list_agents(self, owner_user_id: Optional[str] = None) -> list[dict]:
        """List agents.

        Args:
            owner_user_id: Optional filter by owner user ID

        Returns:
            List of agent dictionaries
        """
        if owner_user_id:
            agents = self.db.fetchall(
                """
                SELECT agent_id, agent_name, agent_type, owner_user_id, is_active, created_at
                FROM agents WHERE owner_user_id = %s
                ORDER BY created_at DESC
                """,
                (owner_user_id,),
            )
        else:
            agents = self.db.fetchall(
                """
                SELECT agent_id, agent_name, agent_type, owner_user_id, is_active, created_at
                FROM agents
                ORDER BY created_at DESC
                """
            )

        return agents

    def update_agent(
        self,
        agent_id: str,
        agent_name: Optional[str] = None,
        config: Optional[dict] = None,
        is_active: Optional[bool] = None,
    ) -> bool:
        """Update agent.

        Args:
            agent_id: Agent ID
            agent_name: Optional new agent name
            config: Optional new configuration
            is_active: Optional active status

        Returns:
            True if agent was updated, False if not found
        """
        updates = []
        params = []

        if agent_name is not None:
            updates.append("agent_name = %s")
            params.append(agent_name)

        if config is not None:
            updates.append("config = %s")
            params.append(str(config))

        if is_active is not None:
            updates.append("is_active = %s")
            params.append(is_active)

        if not updates:
            return False

        updates.append("updated_at = %s")
        params.append(datetime.now(timezone.utc))
        params.append(agent_id)

        query = f"UPDATE agents SET {', '.join(updates)} WHERE agent_id = %s"
        rowcount = self.db.execute(query, tuple(params))

        if rowcount > 0:
            logger.info(f"Updated agent: {agent_id}")
            return True

        return False

    def delete_agent(self, agent_id: str) -> bool:
        """Delete agent.

        Args:
            agent_id: Agent ID

        Returns:
            True if agent was deleted, False if not found
        """
        rowcount = self.db.execute("DELETE FROM agents WHERE agent_id = %s", (agent_id,))

        if rowcount > 0:
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
        agent = self.db.fetchone(
            "SELECT owner_user_id FROM agents WHERE agent_id = %s", (agent_id,)
        )

        if not agent:
            return False

        return agent["owner_user_id"] == user_id
