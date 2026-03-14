"""Integration tests for sessions API."""

import pytest
from fastapi.testclient import TestClient

from api.main import app
from api.database import get_db_session
from api.repositories import UserRepository


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


class TestCreateSession:
    """Test session creation endpoint."""

    def test_create_session_success(self, client, auth_headers):
        """Test successful session creation."""
        response = client.post(
            "/sessions",
            headers=auth_headers,
            json={
                "metadata": {"test": "data"},
            },
        )

        assert response.status_code == 201
        data = response.json()
        assert "session_id" in data
        assert data["status"] == "active"
        assert data["event_count"] == 0
        assert data["metadata"] == {"test": "data"}


class TestListSessions:
    """Test list sessions endpoint."""

    def test_list_sessions_success(self, client, auth_headers):
        """Test successful session listing."""
        # Create a session first
        client.post(
            "/sessions",
            headers=auth_headers,
            json={"metadata": {"test": "data"}},
        )

        # List sessions
        response = client.get("/sessions", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert "sessions" in data
        assert len(data["sessions"]) > 0


class TestGetSession:
    """Test get session endpoint."""

    def test_get_session_success(self, client, auth_headers):
        """Test successful session retrieval."""
        # Create a session
        create_response = client.post(
            "/sessions",
            headers=auth_headers,
            json={"metadata": {"test": "data"}},
        )
        session_id = create_response.json()["session_id"]

        # Get session
        response = client.get(f"/sessions/{session_id}", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert data["session_id"] == session_id
        assert data["status"] == "active"

    def test_get_session_not_found(self, client, auth_headers):
        """Test get non-existent session."""
        response = client.get("/sessions/nonexistent", headers=auth_headers)

        assert response.status_code == 404

    def test_get_session_unauthorized(self, client, auth_headers, db_session):
        """Test get session owned by another user."""
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
                "password_hash": hash_password("password123"),
                "is_active": 1,
            }
        )

        # Login as other user
        response = client.post(
            "/auth/login",
            json={
                "username": "otheruser",
                "password": "password123",
            },
        )
        other_token = response.json()["access_token"]
        other_headers = {"Authorization": f"Bearer {other_token}"}

        # Create session as other user
        create_response = client.post(
            "/sessions",
            headers=other_headers,
            json={"metadata": {}},
        )
        session_id = create_response.json()["session_id"]

        # Try to get session as first user
        response = client.get(f"/sessions/{session_id}", headers=auth_headers)

        assert response.status_code == 404

        # Clean up
        repo.delete(other_user.user_id)
        db_session.commit()


class TestUpdateSession:
    """Test update session endpoint."""

    def test_update_session_success(self, client, auth_headers):
        """Test successful session update."""
        # Create a session
        create_response = client.post(
            "/sessions",
            headers=auth_headers,
            json={"metadata": {"test": "data"}},
        )
        session_id = create_response.json()["session_id"]

        # Update session
        response = client.put(
            f"/sessions/{session_id}",
            headers=auth_headers,
            json={"title": "Updated Title", "metadata": {"updated": True}},
        )

        assert response.status_code == 200
        data = response.json()
        assert data["session_id"] == session_id
        assert data["title"] == "Updated Title"
        assert data["metadata"] == {"updated": True}

    def test_update_session_not_found(self, client, auth_headers):
        """Test update non-existent session."""
        response = client.put(
            "/sessions/nonexistent",
            headers=auth_headers,
            json={"title": "Updated"},
        )

        assert response.status_code == 404

    def test_update_session_unauthorized(self, client, auth_headers, db_session):
        """Test update session owned by another user."""
        from core.auth.password import hash_password
        from uuid import uuid4

        repo = UserRepository(lambda: db_session)

        # Clean up first
        existing = repo.get_by_username("updateuser")
        if existing:
            repo.delete(existing.user_id)
            db_session.commit()

        other_user = repo.create(
            {
                "user_id": str(uuid4()),
                "username": "updateuser",
                "email": "update@example.com",
                "password_hash": hash_password("password123"),
                "is_active": 1,
            }
        )

        # Login as other user
        response = client.post(
            "/auth/login",
            json={
                "username": "updateuser",
                "password": "password123",
            },
        )
        other_token = response.json()["access_token"]
        other_headers = {"Authorization": f"Bearer {other_token}"}

        # Create session as other user
        create_response = client.post(
            "/sessions",
            headers=other_headers,
            json={"metadata": {}},
        )
        session_id = create_response.json()["session_id"]

        # Try to update session as first user
        response = client.put(
            f"/sessions/{session_id}",
            headers=auth_headers,
            json={"title": "Updated"},
        )

        assert response.status_code == 404

        # Clean up
        repo.delete(other_user.user_id)
        db_session.commit()


class TestDeleteSession:
    """Test delete session endpoint."""

    def test_delete_session_success(self, client, auth_headers):
        """Test successful session deletion."""
        # Create a session
        create_response = client.post(
            "/sessions",
            headers=auth_headers,
            json={"metadata": {}},
        )
        session_id = create_response.json()["session_id"]

        # Delete session
        response = client.delete(f"/sessions/{session_id}", headers=auth_headers)

        assert response.status_code == 204

        # Verify deleted
        get_response = client.get(f"/sessions/{session_id}", headers=auth_headers)
        assert get_response.status_code == 404

    def test_delete_session_not_found(self, client, auth_headers):
        """Test delete non-existent session."""
        response = client.delete("/sessions/nonexistent", headers=auth_headers)

        assert response.status_code == 404


class TestCloseSession:
    """Test close session endpoint."""

    def test_close_session_success(self, client, auth_headers):
        """Test successful session closure."""
        # Create a session
        create_response = client.post(
            "/sessions",
            headers=auth_headers,
            json={"metadata": {"test": "data"}},
        )
        session_id = create_response.json()["session_id"]

        # Close session
        response = client.post(f"/sessions/{session_id}/close", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert data["session_id"] == session_id
        assert data["status"] == "closed"
