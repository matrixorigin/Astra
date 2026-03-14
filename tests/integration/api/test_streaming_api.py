"""Integration tests for streaming API with real database."""

import uuid

import pytest
from starlette.testclient import TestClient

from tests.conftest import parse_sse_events


@pytest.fixture
def client():
    """Create test client."""
    from api.main import app

    return TestClient(app)


@pytest.fixture
def auth_token(client):
    """Get auth token by registering and logging in."""
    import time

    username = f"streamuser_{str(uuid.uuid4())}"

    # Register
    client.post(
        "/auth/register",
        json={"username": username, "email": f"{username}@test.com", "password": "testpass123"},
    )

    # Login
    response = client.post("/auth/login", json={"username": username, "password": "testpass123"})
    return response.json()["access_token"]


@pytest.fixture
def test_session(client, auth_token):
    """Create a test session."""
    headers = {"Authorization": f"Bearer {auth_token}"}

    # Create session
    response = client.post("/sessions", headers=headers, json={"metadata": {"test": "streaming"}})
    assert response.status_code == 201
    return response.json()["session_id"]


def test_stream_chat_session_not_found(client, auth_token):
    """Test streaming with non-existent session returns SSE error."""
    headers = {"Authorization": f"Bearer {auth_token}"}

    response = client.post(
        "/chat/stream",
        headers=headers,
        json={"session_id": "nonexistent_session", "message": "Hello"},
    )

    # SSE endpoints always return 200 with text/event-stream
    assert response.status_code == 200
    assert "text/event-stream" in response.headers["content-type"]
    events = parse_sse_events(response.text)
    assert len(events) >= 1
    err = events[0]
    assert err["type"] == "error"
    assert err["code"] == "NOT_FOUND"
    assert "not found" in err["message"].lower()


def test_stream_chat_unauthorized(client):
    """Test streaming without authentication returns SSE error."""
    response = client.post("/chat/stream", json={"session_id": "sess_123", "message": "Hello"})

    # SSE endpoints always return 200 with text/event-stream (even auth errors)
    assert response.status_code == 200
    assert "text/event-stream" in response.headers["content-type"]
    events = parse_sse_events(response.text)
    assert len(events) >= 1
    assert events[0]["type"] == "error"
    assert events[0]["code"] == "AUTH_ERROR"
