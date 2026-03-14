"""Integration tests for events API."""

import pytest
from fastapi.testclient import TestClient

from api.main import app
from api.database import get_db_session
from api.repositories.user_repository import UserRepository


@pytest.fixture
def client():
    """Create test client."""
    return TestClient(app)


@pytest.fixture
def db_session():
    """Get database session."""
    session = next(get_db_session())
    yield session
    session.close()


# auth_headers fixture now provided by tests/integration/conftest.py


@pytest.fixture
def test_session(client, auth_headers):
    """Create test session."""
    response = client.post(
        "/sessions",
        headers=auth_headers,
        json={"metadata": {}},
    )
    return response.json()["session_id"]


class TestCreateEvent:
    """Test event creation endpoint."""

    def test_create_user_query_event(self, client, auth_headers, test_session):
        """Test creating a user query event."""
        response = client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": test_session,
                "event_type": "user_query",
                "content": "What is the weather?",
                "metadata": {"source": "cli"},
            },
        )

        assert response.status_code == 201
        data = response.json()
        assert "event_id" in data
        assert data["event_type"] == "user_query"
        assert data["content"] == "What is the weather?"

    def test_create_event_session_not_found(self, client, auth_headers):
        """Test creating event with non-existent session."""
        response = client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": "nonexistent",
                "event_type": "user_query",
                "content": "test",
            },
        )

        assert response.status_code == 404

    def test_create_event_unauthorized(self, client, auth_headers, db_session):
        """Test creating event for session owned by another user."""
        # Create another user
        from core.auth.password import hash_password
        from uuid import uuid4

        repo = UserRepository(lambda: db_session)

        # Clean up first
        existing = repo.get_by_username("otheruser")
        if existing:
            repo.delete(existing.user_id)
            db_session.commit()

        other_user = repo.create(
            {
                "user_id": str(uuid4()),
                "username": "otheruser",
                "email": "other@example.com",
                "password_hash": hash_password("otherpass123"),
                "is_active": 1,
            }
        )

        # Login as other user
        response = client.post(
            "/auth/login",
            json={
                "username": "otheruser",
                "password": "otherpass123",
            },
        )
        other_token = response.json()["access_token"]
        other_headers = {"Authorization": f"Bearer {other_token}"}

        # Create session as other user
        session_response = client.post(
            "/sessions",
            headers=other_headers,
            json={"metadata": {}},
        )
        session_id = session_response.json()["session_id"]

        # Try to create event as first user
        response = client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": session_id,
                "event_type": "user_query",
                "content": "test",
            },
        )

        assert response.status_code == 404

        # Clean up
        repo.delete(other_user.user_id)
        db_session.commit()


class TestListEvents:
    """Test list events endpoint."""

    def test_list_events_success(self, client, auth_headers, test_session):
        """Test successful event listing."""
        # Create an event
        client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": test_session,
                "event_type": "user_query",
                "content": "test",
            },
        )

        # List events
        response = client.get(
            f"/events?session_id={test_session}",
            headers=auth_headers,
        )

        assert response.status_code == 200
        data = response.json()
        assert "events" in data
        assert len(data["events"]) > 0


class TestGetEvent:
    """Test get event endpoint."""

    def test_get_event_success(self, client, auth_headers, test_session):
        """Test successful event retrieval."""
        # Create an event
        create_response = client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": test_session,
                "event_type": "user_query",
                "content": "test",
            },
        )
        event_id = create_response.json()["event_id"]

        # Get event
        response = client.get(f"/events/{event_id}", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert data["event_id"] == event_id
        assert data["event_type"] == "user_query"

    def test_get_event_not_found(self, client, auth_headers):
        """Test get non-existent event."""
        response = client.get("/events/nonexistent", headers=auth_headers)

        assert response.status_code == 404


class TestCausalChain:
    """Test causal chain endpoint."""

    def test_get_causal_chain(self, client, auth_headers, test_session):
        """Test getting causal chain."""
        # Create first event
        event1_response = client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": test_session,
                "event_type": "user_query",
                "content": "First message",
            },
        )
        event1 = event1_response.json()

        # Create second event with parent
        event2_response = client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": test_session,
                "event_type": "llm_response",
                "content": "Response",
                "parent_event_id": event1["event_id"],
                "causal_chain_id": event1["causal_chain_id"],
            },
        )

        # Get causal chain
        response = client.get(
            f"/events/causal-chain/{event1['causal_chain_id']}",
            headers=auth_headers,
        )

        assert response.status_code == 200
        data = response.json()
        assert len(data) == 2
        assert data[0]["event_id"] == event1["event_id"]
        assert data[1]["event_id"] == event2_response.json()["event_id"]


class TestSessionEvents:
    """Test session events endpoint."""

    def test_get_session_events(self, client, auth_headers, test_session):
        """Test getting session events."""
        # Create events
        for i in range(3):
            client.post(
                "/events",
                headers=auth_headers,
                json={
                    "session_id": test_session,
                    "event_type": "user_query",
                    "content": f"Message {i}",
                },
            )

        # Get session events
        response = client.get(
            f"/events/session/{test_session}",
            headers=auth_headers,
        )

        assert response.status_code == 200
        data = response.json()
        assert len(data["events"]) == 3
        assert data["total"] == 3


class TestDeleteEvent:
    """Test delete event endpoint."""

    def test_delete_event_success(self, client, auth_headers, test_session):
        """Test successful event deletion."""
        # Create an event
        create_response = client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": test_session,
                "event_type": "user_query",
                "content": "test",
            },
        )
        event_id = create_response.json()["event_id"]

        # Delete event
        response = client.delete(f"/events/{event_id}", headers=auth_headers)

        assert response.status_code == 204

        # Verify deleted
        get_response = client.get(f"/events/{event_id}", headers=auth_headers)
        assert get_response.status_code == 404

    def test_delete_event_not_found(self, client, auth_headers):
        """Test deleting non-existent event."""
        response = client.delete("/events/nonexistent", headers=auth_headers)

        assert response.status_code == 404
