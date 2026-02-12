"""Integration tests for agents API."""

from unittest.mock import MagicMock, patch

import pytest


@pytest.fixture
def mock_agent_manager():
    """Mock agent manager for testing."""
    with patch("api.routers.agents.AgentManager") as mock_class:
        manager = MagicMock()
        mock_class.return_value = manager
        yield manager


@pytest.fixture
def mock_current_user():
    """Mock current user for testing."""
    return {
        "user_id": "user_123",
        "username": "testuser",
        "email": "test@example.com",
    }


@pytest.fixture
def client():
    """Create test client."""
    from fastapi.testclient import TestClient
    from api.main import app

    return TestClient(app)


@pytest.fixture
def auth_headers():
    """Create authentication headers with valid token."""
    from core.auth.jwt_manager import create_access_token

    token = create_access_token({"sub": "user_123", "username": "testuser"})
    return {"Authorization": f"Bearer {token}"}


class TestCreateAgent:
    """Test agent creation endpoint."""

    def test_create_agent_success(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test successful agent creation."""
        mock_agent_manager.create_agent.return_value = {
            "agent_id": "agent_123",
            "agent_name": "Test Agent",
            "agent_type": "chatbot",
            "owner_user_id": "user_123",
            "config": {"model": "gpt-4"},
        }

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.post(
                    "/agents",
                    json={
                        "agent_name": "Test Agent",
                        "agent_type": "chatbot",
                        "config": {"model": "gpt-4"},
                    },
                    headers=auth_headers,
                )

        assert response.status_code == 201
        data = response.json()
        assert data["agent_name"] == "Test Agent"
        assert data["agent_type"] == "chatbot"
        assert "agent_id" in data

    def test_create_agent_unauthorized(self, client):
        """Test creating agent without authentication."""
        response = client.post(
            "/agents",
            json={
                "agent_name": "Test Agent",
                "agent_type": "chatbot",
            },
        )

        assert response.status_code == 403  # No auth header

    def test_create_agent_invalid_type(self, client, auth_headers, mock_current_user):
        """Test creating agent with invalid type."""
        with patch("api.dependencies.UserManager") as mock_um:
            mock_um.return_value.get_user_by_id.return_value = mock_current_user
            
            response = client.post(
                "/agents",
                json={
                    "agent_name": "Test Agent",
                    "agent_type": "invalid_type",
                },
                headers=auth_headers,
            )

        assert response.status_code == 422


class TestListAgents:
    """Test listing agents endpoint."""

    def test_list_agents_success(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test successful agent listing."""
        mock_agent_manager.list_agents.return_value = [
            {
                "agent_id": "agent_1",
                "agent_name": "Agent 1",
                "agent_type": "chatbot",
                "owner_user_id": "user_123",
                "is_active": True,
            },
            {
                "agent_id": "agent_2",
                "agent_name": "Agent 2",
                "agent_type": "assistant",
                "owner_user_id": "user_123",
                "is_active": True,
            },
        ]

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.get("/agents", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert len(data) == 2
        assert data[0]["agent_id"] == "agent_1"

    def test_list_agents_empty(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test listing agents when none exist."""
        mock_agent_manager.list_agents.return_value = []

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.get("/agents", headers=auth_headers)

        assert response.status_code == 200
        assert response.json() == []


class TestGetAgent:
    """Test getting single agent endpoint."""

    def test_get_agent_success(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test successful agent retrieval."""
        mock_agent_manager.get_agent.return_value = {
            "agent_id": "agent_123",
            "agent_name": "Test Agent",
            "agent_type": "chatbot",
            "owner_user_id": "user_123",
            "config": None,
            "is_active": True,
        }

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.get("/agents/agent_123", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert data["agent_id"] == "agent_123"

    def test_get_agent_not_found(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test getting non-existent agent."""
        mock_agent_manager.get_agent.return_value = None

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.get("/agents/nonexistent", headers=auth_headers)

        assert response.status_code == 404

    def test_get_agent_forbidden(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test getting agent owned by another user."""
        mock_agent_manager.get_agent.return_value = {
            "agent_id": "agent_123",
            "agent_name": "Test Agent",
            "agent_type": "chatbot",
            "owner_user_id": "other_user",
            "is_active": True,
        }

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.get("/agents/agent_123", headers=auth_headers)

        assert response.status_code == 403


class TestUpdateAgent:
    """Test updating agent endpoint."""

    def test_update_agent_success(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test successful agent update."""
        mock_agent_manager.verify_agent_owner.return_value = True
        mock_agent_manager.update_agent.return_value = True
        mock_agent_manager.get_agent.return_value = {
            "agent_id": "agent_123",
            "agent_name": "Updated Agent",
            "agent_type": "chatbot",
            "owner_user_id": "user_123",
            "config": {"model": "gpt-4"},
            "is_active": True,
        }

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.put(
                    "/agents/agent_123",
                    json={"agent_name": "Updated Agent"},
                    headers=auth_headers,
                )

        assert response.status_code == 200
        data = response.json()
        assert data["agent_name"] == "Updated Agent"

    def test_update_agent_not_owner(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test updating agent not owned by user."""
        mock_agent_manager.verify_agent_owner.return_value = False

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.put(
                    "/agents/agent_123",
                    json={"agent_name": "Updated Agent"},
                    headers=auth_headers,
                )

        assert response.status_code == 404


class TestDeleteAgent:
    """Test deleting agent endpoint."""

    def test_delete_agent_success(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test successful agent deletion."""
        mock_agent_manager.verify_agent_owner.return_value = True
        mock_agent_manager.delete_agent.return_value = True

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.delete("/agents/agent_123", headers=auth_headers)

        assert response.status_code == 204

    def test_delete_agent_not_owner(self, client, auth_headers, mock_agent_manager, mock_current_user):
        """Test deleting agent not owned by user."""
        mock_agent_manager.verify_agent_owner.return_value = False

        with patch("api.routers.agents.get_agent_manager", return_value=mock_agent_manager):
            with patch("api.dependencies.UserManager") as mock_um:
                mock_um.return_value.get_user_by_id.return_value = mock_current_user
                
                response = client.delete("/agents/agent_123", headers=auth_headers)

        assert response.status_code == 404
