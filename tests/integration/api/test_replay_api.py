"""Integration tests for replay API."""

import pytest
from uuid import uuid4
from fastapi.testclient import TestClient

from api.main import app
from api.database import get_db_session
from api.repositories.user_repository import UserRepository


@pytest.fixture
def client(db_session):
    """Create test client bound to the shared test DB session."""

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
    """Create test session with events."""
    # Create session
    response = client.post(
        "/sessions",
        headers=auth_headers,
        json={"metadata": {}},
    )
    assert response.status_code == 201, response.text
    session_id = response.json()["session_id"]

    # Create some events
    for i in range(3):
        client.post(
            "/events",
            headers=auth_headers,
            json={
                "session_id": session_id,
                "event_type": "user_query",
                "content": f"Message {i}",
            },
        )

    return session_id


class TestReplaySession:
    """Test session replay endpoint."""

    def test_replay_session_success(self, client, auth_headers, test_session):
        """Test successful session replay."""
        response = client.post(
            f"/sessions/{test_session}/replay",
            headers=auth_headers,
            json={"mock_mode": True},
        )

        assert response.status_code == 201
        data = response.json()
        assert "replay_id" in data
        assert data["session_id"] == test_session
        assert data["status"] == "completed"
        assert data["mock_mode"] is True

    def test_replay_session_with_sandbox(self, client, auth_headers, test_session):
        """Test replay with sandbox."""
        response = client.post(
            f"/sessions/{test_session}/replay",
            headers=auth_headers,
            json={"sandbox_name": "test_sandbox", "mock_mode": True},
        )

        assert response.status_code == 201
        data = response.json()
        assert data["sandbox_name"] == "test_sandbox"

    def test_replay_session_not_found(self, client, auth_headers):
        """Test replay non-existent session."""
        response = client.post(
            "/sessions/nonexistent/replay",
            headers=auth_headers,
            json={"mock_mode": True},
        )

        assert response.status_code == 404

    def test_replay_session_unauthorized(self, client, auth_headers, db_session):
        """Test replay session owned by another user."""
        # Create another user with unique name
        from core.auth.password import hash_password

        uid = uuid4().hex
        other_username = f"otherreplay_{uid}"

        repo = UserRepository(lambda: db_session)

        other_user = repo.create(
            {
                "user_id": str(uuid4()),
                "username": other_username,
                "email": f"otherreplay_{uid}@example.com",
                "password_hash": hash_password("password123"),
                "is_active": 1,
            }
        )

        # Login as other user
        response = client.post(
            "/auth/login",
            json={
                "username": other_username,
                "password": "password123",
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
        assert session_response.status_code == 201, session_response.text
        session_id = session_response.json()["session_id"]

        # Try to replay as first user
        response = client.post(
            f"/sessions/{session_id}/replay",
            headers=auth_headers,
            json={"mock_mode": True},
        )

        assert response.status_code == 403

        # Clean up
        repo.delete(other_user.user_id)
        db_session.commit()


class TestCompareReplay:
    """Test replay comparison endpoint."""

    def test_compare_replay_success(self, client, auth_headers, test_session):
        """Test successful replay comparison."""
        response = client.get(
            f"/sessions/{test_session}/replay/compare",
            headers=auth_headers,
        )

        assert response.status_code == 200
        data = response.json()
        assert "session_id" in data
        assert "original_event_count" in data
        assert "replay_event_count" in data
        assert "match" in data

    def test_compare_replay_not_found(self, client, auth_headers):
        """Test compare non-existent session."""
        response = client.get(
            "/sessions/nonexistent/replay/compare",
            headers=auth_headers,
        )

        assert response.status_code == 404
