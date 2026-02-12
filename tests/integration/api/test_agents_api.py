"""Integration tests for agents API with real database."""

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client():
    """Create test client."""
    from api.main import app
    return TestClient(app)


@pytest.fixture
def auth_token(client):
    """Get auth token by registering and logging in."""
    # Register
    username = f"testuser_{pytest.test_id}"
    client.post("/auth/register", json={
        "username": username,
        "email": f"{username}@test.com",
        "password": "testpass123"
    })
    
    # Login
    response = client.post("/auth/login", json={
        "username": username,
        "password": "testpass123"
    })
    return response.json()["access_token"]


@pytest.fixture(autouse=True)
def setup_test_id():
    """Generate unique test ID."""
    import time
    pytest.test_id = str(int(time.time() * 1000))


def test_create_and_list_agents(client, auth_token):
    """Test creating and listing agents."""
    headers = {"Authorization": f"Bearer {auth_token}"}
    
    # Create agent
    response = client.post("/agents", headers=headers, json={
        "name": "Test Agent",
        "agent_config": {"model": "gpt-4"}
    })
    assert response.status_code == 201
    agent = response.json()
    assert agent["name"] == "Test Agent"
    agent_id = agent["agent_id"]
    
    # List agents
    response = client.get("/agents", headers=headers)
    assert response.status_code == 200
    agents = response.json()["agents"]
    assert len(agents) >= 1
    assert any(a["agent_id"] == agent_id for a in agents)


def test_get_agent(client, auth_token):
    """Test getting agent by ID."""
    headers = {"Authorization": f"Bearer {auth_token}"}
    
    # Create agent
    response = client.post("/agents", headers=headers, json={
        "name": "Get Test",
        "agent_config": {}
    })
    agent_id = response.json()["agent_id"]
    
    # Get agent
    response = client.get(f"/agents/{agent_id}", headers=headers)
    assert response.status_code == 200
    agent = response.json()
    assert agent["agent_id"] == agent_id
    assert agent["name"] == "Get Test"


def test_update_agent(client, auth_token):
    """Test updating agent."""
    headers = {"Authorization": f"Bearer {auth_token}"}
    
    # Create agent
    response = client.post("/agents", headers=headers, json={
        "name": "Original",
        "agent_config": {}
    })
    agent_id = response.json()["agent_id"]
    
    # Update agent
    response = client.put(f"/agents/{agent_id}", headers=headers, json={
        "name": "Updated",
        "agent_config": {"new": "config"}
    })
    assert response.status_code == 200
    agent = response.json()
    assert agent["name"] == "Updated"
    assert agent["agent_config"]["new"] == "config"


def test_delete_agent(client, auth_token):
    """Test deleting agent."""
    headers = {"Authorization": f"Bearer {auth_token}"}
    
    # Create agent
    response = client.post("/agents", headers=headers, json={
        "name": "To Delete",
        "agent_config": {}
    })
    agent_id = response.json()["agent_id"]
    
    # Delete agent
    response = client.delete(f"/agents/{agent_id}", headers=headers)
    assert response.status_code == 204
    
    # Verify deleted
    response = client.get(f"/agents/{agent_id}", headers=headers)
    assert response.status_code == 404


def test_agent_not_found(client, auth_token):
    """Test getting non-existent agent."""
    headers = {"Authorization": f"Bearer {auth_token}"}
    response = client.get("/agents/nonexistent", headers=headers)
    assert response.status_code == 404


def test_unauthorized_access(client):
    """Test accessing agents without auth."""
    response = client.get("/agents")
    assert response.status_code == 403  # FastAPI returns 403 for missing auth
