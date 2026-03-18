"""Integration tests for context API."""

import pytest
from fastapi.testclient import TestClient
from uuid import uuid4

from api.main import app
from api.database import get_db_session
from api.repositories.user_repository import UserRepository


@pytest.fixture
def client(db_session):
    def override_get_db():
        try:
            yield db_session
        finally:
            pass

    app.dependency_overrides[get_db_session] = override_get_db
    try:
        yield TestClient(app)
    finally:
        app.dependency_overrides.pop(get_db_session, None)


# auth_headers fixture now provided by tests/integration/conftest.py


@pytest.fixture
def test_session(client, auth_headers):
    """Create test session with event."""
    # Create session
    response = client.post(
        "/sessions",
        headers=auth_headers,
        json={"metadata": {}},
    )
    session_id = response.json()["session_id"]

    # Create event
    response = client.post(
        "/events",
        headers=auth_headers,
        json={
            "session_id": session_id,
            "event_type": "user_query",
            "content": "Test message",
        },
    )
    event_id = response.json()["event_id"]

    return {"session_id": session_id, "event_id": event_id}


def test_create_snapshot_success(client, auth_headers, test_session):
    """Test successful snapshot creation."""
    response = client.post(
        "/context",
        headers=auth_headers,
        json={
            "session_id": test_session["session_id"],
            "event_id": test_session["event_id"],
            "context_data": {
                "system_prompt": "You are a helpful assistant",
                "skill_definitions": ["skill1", "skill2"],
                "selected_events": ["event1"],
                "code_context": {"file": "test.py"},
                "documentation": {"doc": "test"},
                "total_tokens": 100,
                "token_budget": {"max": 4000},
                "assembly_time_ms": 50,
                "relevance_scores": {"score": 0.9},
                "task_type": "code_review",
            },
        },
    )

    if response.status_code != 201:
        print(f"Error: {response.json()}")

    assert response.status_code == 201
    data = response.json()
    assert "context_capture_id" in data
    assert data["session_id"] == test_session["session_id"]
    assert data["event_id"] == test_session["event_id"]
    assert "system_prompt" in data["context_data"]


def test_get_snapshot_success(client, auth_headers, test_session):
    """Test successful snapshot retrieval."""
    # Create snapshot
    create_response = client.post(
        "/context",
        headers=auth_headers,
        json={
            "session_id": test_session["session_id"],
            "event_id": test_session["event_id"],
            "context_data": {"system_prompt": "test", "task_type": "test"},
        },
    )
    context_capture_id = create_response.json()["context_capture_id"]

    # Get snapshot
    response = client.get(f"/context/{context_capture_id}", headers=auth_headers)

    assert response.status_code == 200
    data = response.json()
    assert data["context_capture_id"] == context_capture_id
    assert data["context_data"]["system_prompt"] == "test"


def test_list_snapshots_success(client, auth_headers, test_session):
    """Test successful snapshot listing."""
    # Create snapshots
    for i in range(3):
        client.post(
            "/context",
            headers=auth_headers,
            json={
                "session_id": test_session["session_id"],
                "event_id": test_session["event_id"],
                "context_data": {"system_prompt": f"test {i}", "task_type": "test"},
            },
        )

    # List snapshots
    response = client.get("/context", headers=auth_headers)

    assert response.status_code == 200
    data = response.json()
    assert "snapshots" in data
    assert data["total"] >= 3


def test_get_snapshot_not_found(client, auth_headers):
    """Test get non-existent snapshot."""
    response = client.get("/context/nonexistent", headers=auth_headers)
    assert response.status_code == 404
