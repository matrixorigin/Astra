"""Tests for agent manager."""

from datetime import datetime, timezone
from unittest.mock import MagicMock

import pytest

from core.agent.agent_manager import AgentManager


@pytest.fixture
def mock_db():
    """Mock database for testing."""
    return MagicMock()


@pytest.fixture
def agent_manager(mock_db):
    """Create agent manager with mock database."""
    return AgentManager(mock_db)


class TestCreateAgent:
    """Test agent creation."""

    def test_create_agent_success(self, agent_manager, mock_db):
        """Test successful agent creation."""
        mock_db.fetchone.return_value = {"user_id": "user_123"}  # Owner exists
        mock_db.execute.return_value = 1

        agent = agent_manager.create_agent(
            agent_name="Test Agent",
            owner_user_id="user_123",
            agent_type="chatbot",
            config={"model": "gpt-4"},
        )

        assert agent["agent_name"] == "Test Agent"
        assert agent["owner_user_id"] == "user_123"
        assert agent["agent_type"] == "chatbot"
        assert agent["config"] == {"model": "gpt-4"}
        assert "agent_id" in agent

    def test_create_agent_owner_not_found(self, agent_manager, mock_db):
        """Test creating agent with non-existent owner."""
        mock_db.fetchone.return_value = None

        with pytest.raises(ValueError, match="User .* does not exist"):
            agent_manager.create_agent(
                agent_name="Test Agent",
                owner_user_id="nonexistent",
            )

    def test_create_agent_without_config(self, agent_manager, mock_db):
        """Test creating agent without config."""
        mock_db.fetchone.return_value = {"user_id": "user_123"}
        mock_db.execute.return_value = 1

        agent = agent_manager.create_agent(
            agent_name="Test Agent",
            owner_user_id="user_123",
        )

        assert agent["config"] is None


class TestGetAgent:
    """Test getting agent."""

    def test_get_agent_found(self, agent_manager, mock_db):
        """Test getting agent when found."""
        mock_db.fetchone.return_value = {
            "agent_id": "agent_123",
            "agent_name": "Test Agent",
            "agent_type": "chatbot",
            "owner_user_id": "user_123",
            "config": None,
            "is_active": True,
            "created_at": datetime.now(timezone.utc),
        }

        agent = agent_manager.get_agent("agent_123")

        assert agent is not None
        assert agent["agent_id"] == "agent_123"

    def test_get_agent_not_found(self, agent_manager, mock_db):
        """Test getting agent when not found."""
        mock_db.fetchone.return_value = None

        agent = agent_manager.get_agent("nonexistent")

        assert agent is None


class TestListAgents:
    """Test listing agents."""

    def test_list_agents_all(self, agent_manager, mock_db):
        """Test listing all agents."""
        mock_db.fetchall.return_value = [
            {
                "agent_id": "agent_1",
                "agent_name": "Agent 1",
                "agent_type": "chatbot",
                "owner_user_id": "user_123",
                "is_active": True,
                "created_at": datetime.now(timezone.utc),
            },
            {
                "agent_id": "agent_2",
                "agent_name": "Agent 2",
                "agent_type": "assistant",
                "owner_user_id": "user_456",
                "is_active": True,
                "created_at": datetime.now(timezone.utc),
            },
        ]

        agents = agent_manager.list_agents()

        assert len(agents) == 2
        assert agents[0]["agent_id"] == "agent_1"
        assert agents[1]["agent_id"] == "agent_2"

    def test_list_agents_by_owner(self, agent_manager, mock_db):
        """Test listing agents by owner."""
        mock_db.fetchall.return_value = [
            {
                "agent_id": "agent_1",
                "agent_name": "Agent 1",
                "agent_type": "chatbot",
                "owner_user_id": "user_123",
                "is_active": True,
                "created_at": datetime.now(timezone.utc),
            },
        ]

        agents = agent_manager.list_agents(owner_user_id="user_123")

        assert len(agents) == 1
        assert agents[0]["owner_user_id"] == "user_123"

    def test_list_agents_empty(self, agent_manager, mock_db):
        """Test listing agents when none exist."""
        mock_db.fetchall.return_value = []

        agents = agent_manager.list_agents()

        assert agents == []


class TestUpdateAgent:
    """Test updating agent."""

    def test_update_agent_name(self, agent_manager, mock_db):
        """Test updating agent name."""
        mock_db.execute.return_value = 1

        success = agent_manager.update_agent("agent_123", agent_name="New Name")

        assert success is True
        mock_db.execute.assert_called_once()

    def test_update_agent_config(self, agent_manager, mock_db):
        """Test updating agent config."""
        mock_db.execute.return_value = 1

        success = agent_manager.update_agent("agent_123", config={"model": "gpt-4"})

        assert success is True

    def test_update_agent_is_active(self, agent_manager, mock_db):
        """Test updating agent active status."""
        mock_db.execute.return_value = 1

        success = agent_manager.update_agent("agent_123", is_active=False)

        assert success is True

    def test_update_agent_multiple_fields(self, agent_manager, mock_db):
        """Test updating multiple agent fields."""
        mock_db.execute.return_value = 1

        success = agent_manager.update_agent(
            "agent_123",
            agent_name="New Name",
            config={"model": "gpt-4"},
            is_active=False,
        )

        assert success is True

    def test_update_agent_not_found(self, agent_manager, mock_db):
        """Test updating non-existent agent."""
        mock_db.execute.return_value = 0

        success = agent_manager.update_agent("nonexistent", agent_name="New Name")

        assert success is False

    def test_update_agent_no_fields(self, agent_manager, mock_db):
        """Test updating agent with no fields."""
        success = agent_manager.update_agent("agent_123")

        assert success is False


class TestDeleteAgent:
    """Test deleting agent."""

    def test_delete_agent_success(self, agent_manager, mock_db):
        """Test successful agent deletion."""
        mock_db.execute.return_value = 1

        success = agent_manager.delete_agent("agent_123")

        assert success is True
        mock_db.execute.assert_called_once()

    def test_delete_agent_not_found(self, agent_manager, mock_db):
        """Test deleting non-existent agent."""
        mock_db.execute.return_value = 0

        success = agent_manager.delete_agent("nonexistent")

        assert success is False


class TestVerifyAgentOwner:
    """Test verifying agent ownership."""

    def test_verify_agent_owner_true(self, agent_manager, mock_db):
        """Test verifying correct owner."""
        mock_db.fetchone.return_value = {"owner_user_id": "user_123"}

        result = agent_manager.verify_agent_owner("agent_123", "user_123")

        assert result is True

    def test_verify_agent_owner_false(self, agent_manager, mock_db):
        """Test verifying incorrect owner."""
        mock_db.fetchone.return_value = {"owner_user_id": "user_123"}

        result = agent_manager.verify_agent_owner("agent_123", "user_456")

        assert result is False

    def test_verify_agent_owner_agent_not_found(self, agent_manager, mock_db):
        """Test verifying owner for non-existent agent."""
        mock_db.fetchone.return_value = None

        result = agent_manager.verify_agent_owner("nonexistent", "user_123")

        assert result is False
