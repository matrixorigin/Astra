"""Integration tests for streaming API with real database."""

import json
import pytest
from starlette.testclient import TestClient


@pytest.fixture
def client():
    """Create test client."""
    from api.main import app
    return TestClient(app)


@pytest.fixture
def auth_token(client):
    """Get auth token by registering and logging in."""
    import time
    username = f"streamuser_{int(time.time() * 1000)}"
    
    # Register
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


@pytest.fixture
def test_session(client, auth_token):
    """Create a test session."""
    headers = {"Authorization": f"Bearer {auth_token}"}
    
    # Create session
    response = client.post("/sessions", headers=headers, json={
        "metadata": {"test": "streaming"}
    })
    assert response.status_code == 201
    return response.json()["session_id"]


def test_stream_chat_session_not_found(client, auth_token):
    """Test streaming with non-existent session."""
    headers = {"Authorization": f"Bearer {auth_token}"}
    
    response = client.post("/chat/stream", headers=headers, json={
        "session_id": "nonexistent_session",
        "message": "Hello"
    })
    
    assert response.status_code == 404
    assert "not found" in response.json()["detail"].lower()


def test_stream_chat_unauthorized(client):
    """Test streaming without authentication."""
    response = client.post("/chat/stream", json={
        "session_id": "sess_123",
        "message": "Hello"
    })
    
    assert response.status_code == 401  # FastAPI returns 403 for missing auth
