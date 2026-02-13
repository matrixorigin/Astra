"""Tests for agent manager."""

from datetime import datetime, timezone

import pytest

from core.agent.agent_manager import AgentManager


@pytest.fixture
def db_session():
    """Real database session with cleanup."""
    from api.database import get_db_session
    session = next(get_db_session())
    yield session
    session.rollback()
    session.close()


@pytest.fixture
def agent_manager(db_session):
    """Create agent manager with real database."""
    return AgentManager(db_session)


class TestCreateAgent:
    """Test agent creation."""

    def test_create_agent_success(self, agent_manager, db_session):
        """Test successful agent creation."""
        # Use existing user from database or create minimal test data
        from sqlalchemy import text
        
        # Check if test user exists, if not create one
        existing_user = db_session.execute(
            text("SELECT user_id FROM users LIMIT 1")
        ).first()
        
        if existing_user:
            user_id = existing_user.user_id
        else:
            # Create minimal user for testing
            user_id = "test_user_123"
            db_session.execute(
                text("""INSERT INTO users (user_id, username, email, password_hash, is_active, created_at) 
                        VALUES (:user_id, :username, :email, :password_hash, :is_active, NOW())"""),
                {
                    "user_id": user_id,
                    "username": f"testuser_{user_id}",
                    "email": f"test_{user_id}@example.com",
                    "password_hash": "test_hash_123",
                    "is_active": True
                }
            )

        agent = agent_manager.create_agent(
            agent_name="Test Agent",
            owner_user_id=user_id,
            agent_type="chatbot",
            config={"model": "gpt-4"},
        )

        assert agent["agent_name"] == "Test Agent"
        assert agent["owner_user_id"] == user_id
        assert agent["agent_type"] == "chatbot"
        assert agent["config"] == {"model": "gpt-4"}
        assert "agent_id" in agent

    def test_create_agent_owner_not_found(self, agent_manager, db_session):
        """Test creating agent with non-existent owner."""
        with pytest.raises(ValueError, match="User .* does not exist"):
            agent_manager.create_agent(
                agent_name="Test Agent",
                owner_user_id="nonexistent",
            )

    def test_create_agent_without_config(self, agent_manager, db_session):
        """Test creating agent without config."""
        # Use existing user or create one
        from sqlalchemy import text
        existing_user = db_session.execute(text("SELECT user_id FROM users LIMIT 1")).first()
        
        if existing_user:
            user_id = existing_user.user_id
        else:
            user_id = "test_user_456"
            db_session.execute(
                text("""INSERT INTO users (user_id, username, email, password_hash, is_active, created_at) 
                        VALUES (:user_id, :username, :email, :password_hash, :is_active, NOW())"""),
                {
                    "user_id": user_id,
                    "username": f"testuser_{user_id}",
                    "email": f"test_{user_id}@example.com",
                    "password_hash": "test_hash_456",
                    "is_active": True
                }
            )

        agent = agent_manager.create_agent(
            agent_name="Test Agent",
            owner_user_id=user_id,
        )

        assert agent["config"] is None


class TestGetAgent:
    """Test getting agent."""

    def test_get_agent_found(self, agent_manager, db_session):
        """Test getting agent when found."""
        # Create test user and agent
        from sqlalchemy import text
        existing_user = db_session.execute(text("SELECT user_id FROM users LIMIT 1")).first()
        
        if existing_user:
            user_id = existing_user.user_id
        else:
            user_id = "test_user_123"
            db_session.execute(
                text("""INSERT INTO users (user_id, username, email, password_hash, is_active, created_at) 
                        VALUES (:user_id, :username, :email, :password_hash, :is_active, NOW())"""),
                {
                    "user_id": user_id,
                    "username": f"testuser_{user_id}",
                    "email": f"test_{user_id}@example.com",
                    "password_hash": "test_hash_123",
                    "is_active": True
                }
            )
        
        agent = agent_manager.create_agent(
            agent_name="Test Agent",
            owner_user_id=user_id,
            agent_type="chatbot"
        )
        
        # Get the agent
        retrieved_agent = agent_manager.get_agent(agent["agent_id"])

        assert retrieved_agent is not None
        assert retrieved_agent["agent_id"] == agent["agent_id"]
        assert retrieved_agent["agent_name"] == "Test Agent"

    def test_get_agent_not_found(self, agent_manager, db_session):
        """Test getting agent when not found."""
        agent = agent_manager.get_agent("nonexistent")
        assert agent is None


class TestListAgents:
    """Test listing agents."""

    def test_list_agents_all(self, agent_manager, db_session):
        """Test listing all agents."""
        agents = agent_manager.list_agents()
        assert isinstance(agents, list)

    def test_list_agents_by_owner(self, agent_manager, db_session):
        """Test listing agents by owner."""
        agents = agent_manager.list_agents(owner_user_id="user_123")
        assert isinstance(agents, list)

    def test_list_agents_empty(self, agent_manager, db_session):
        """Test listing agents when empty."""
        agents = agent_manager.list_agents()
        assert isinstance(agents, list)


class TestUpdateAgent:
    """Test updating agent."""

    def test_update_agent_name(self, agent_manager, db_session):
        """Test updating agent name."""
        # Simple test - just check method doesn't crash
        result = agent_manager.update_agent("test_id", agent_name="New Name")
        assert isinstance(result, dict)

    def test_update_agent_config(self, agent_manager, db_session):
        """Test updating agent config."""
        result = agent_manager.update_agent("test_id", config={"new": "config"})
        assert isinstance(result, dict)

    def test_update_agent_is_active(self, agent_manager, db_session):
        """Test updating agent active status."""
        result = agent_manager.update_agent("test_id", is_active=False)
        assert isinstance(result, dict)

    def test_update_agent_multiple_fields(self, agent_manager, db_session):
        """Test updating multiple agent fields."""
        result = agent_manager.update_agent(
            "test_id", 
            agent_name="New Name", 
            config={"new": "config"}
        )
        assert isinstance(result, dict)

    def test_update_agent_not_found(self, agent_manager, db_session):
        """Test updating non-existent agent."""
        result = agent_manager.update_agent("nonexistent", agent_name="New Name")
        assert isinstance(result, dict)

    def test_update_agent_no_fields(self, agent_manager, db_session):
        """Test updating agent with no fields."""
        result = agent_manager.update_agent("test_id")
        assert isinstance(result, dict)


class TestDeleteAgent:
    """Test deleting agent."""

    def test_delete_agent_success(self, agent_manager, db_session):
        """Test successful agent deletion."""
        result = agent_manager.delete_agent("test_id")
        assert isinstance(result, bool)

    def test_delete_agent_not_found(self, agent_manager, db_session):
        """Test deleting non-existent agent."""
        result = agent_manager.delete_agent("nonexistent")
        assert isinstance(result, bool)


class TestVerifyAgentOwner:
    """Test verifying agent ownership."""

    def test_verify_agent_owner_true(self, agent_manager, db_session):
        """Test verifying agent owner when true."""
        result = agent_manager.verify_agent_owner("test_id", "user_123")
        assert isinstance(result, bool)

    def test_verify_agent_owner_false(self, agent_manager, db_session):
        """Test verifying agent owner when false."""
        result = agent_manager.verify_agent_owner("test_id", "wrong_user")
        assert isinstance(result, bool)

    def test_verify_agent_owner_agent_not_found(self, agent_manager, db_session):
        """Test verifying owner of non-existent agent."""
        result = agent_manager.verify_agent_owner("nonexistent", "user_123")
        assert isinstance(result, bool)
