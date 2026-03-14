"""Integration tests for decisions API."""

import pytest
from fastapi.testclient import TestClient
from uuid import uuid4

from api.main import app
from api.database import get_db_session
from api.repositories.user_repository import UserRepository


@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def db_session():
    session = next(get_db_session())
    yield session
    session.close()


# auth_headers fixture now provided by tests/integration/conftest.py


@pytest.fixture
def test_data(client, auth_headers):
    """Create session, event, and snapshot."""
    # Create session
    resp = client.post("/sessions", headers=auth_headers, json={"metadata": {}})
    session_id = resp.json()["session_id"]

    # Create event
    resp = client.post(
        "/events",
        headers=auth_headers,
        json={"session_id": session_id, "event_type": "user_query", "content": "test"},
    )
    event_id = resp.json()["event_id"]

    # Create snapshot
    resp = client.post(
        "/context",
        headers=auth_headers,
        json={
            "session_id": session_id,
            "event_id": event_id,
            "context_data": {"system_prompt": "test", "task_type": "test"},
        },
    )
    context_capture_id = resp.json()["context_capture_id"]

    return {
        "session_id": session_id,
        "event_id": event_id,
        "context_capture_id": context_capture_id,
    }


def test_record_decision_success(client, auth_headers, test_data):
    """Test successful decision recording."""
    response = client.post(
        "/decisions",
        headers=auth_headers,
        json={
            "session_id": test_data["session_id"],
            "event_id": test_data["event_id"],
            "context_capture_id": test_data["context_capture_id"],
            "decision_type": "skill_selection",
            "decision_output": {"selected_skill": "code_review"},
            "model_params": {"model": "gpt-4", "temperature": 0.7},
        },
    )

    assert response.status_code == 201
    data = response.json()
    assert "decision_id" in data
    assert data["decision_type"] == "skill_selection"
    assert data["decision_output"]["selected_skill"] == "code_review"


def test_get_decision_success(client, auth_headers, test_data):
    """Test successful decision retrieval."""
    # Record decision
    create_resp = client.post(
        "/decisions",
        headers=auth_headers,
        json={
            "session_id": test_data["session_id"],
            "event_id": test_data["event_id"],
            "context_capture_id": test_data["context_capture_id"],
            "decision_type": "response_generation",
            "decision_output": {"response": "test"},
        },
    )
    decision_id = create_resp.json()["decision_id"]

    # Get decision
    response = client.get(f"/decisions/{decision_id}", headers=auth_headers)

    assert response.status_code == 200
    data = response.json()
    assert data["decision_id"] == decision_id


def test_audit_decision_success(client, auth_headers, test_data):
    """Test decision audit with full context."""
    # Record decision
    create_resp = client.post(
        "/decisions",
        headers=auth_headers,
        json={
            "session_id": test_data["session_id"],
            "event_id": test_data["event_id"],
            "context_capture_id": test_data["context_capture_id"],
            "decision_type": "skill_selection",
            "decision_output": {"skill": "test"},
        },
    )
    decision_id = create_resp.json()["decision_id"]

    # Audit decision
    response = client.get(f"/decisions/{decision_id}/audit", headers=auth_headers)

    assert response.status_code == 200
    data = response.json()
    assert data["decision_id"] == decision_id
    assert "context" in data
    assert data["context"]["system_prompt"] == "test"


def test_list_decisions_success(client, auth_headers, test_data):
    """Test successful decision listing."""
    # Record decisions
    for i in range(3):
        client.post(
            "/decisions",
            headers=auth_headers,
            json={
                "session_id": test_data["session_id"],
                "event_id": test_data["event_id"],
                "context_capture_id": test_data["context_capture_id"],
                "decision_type": "test",
                "decision_output": {"index": i},
            },
        )

    # List decisions
    response = client.get("/decisions", headers=auth_headers)

    assert response.status_code == 200
    data = response.json()
    assert "decisions" in data
    assert data["total"] >= 3


def test_get_decision_not_found(client, auth_headers):
    """Test get non-existent decision."""
    response = client.get("/decisions/nonexistent", headers=auth_headers)
    assert response.status_code == 404
