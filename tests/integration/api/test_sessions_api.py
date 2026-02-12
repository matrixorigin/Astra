"""Integration tests for sessions API."""

from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client():
    """Create test client."""
    from api.main import app
    return TestClient(app)


@pytest.fixture
def mock_session():
    """Mock session object."""
    from datetime import datetime, timezone
    from core.events.session_models import SessionStatus
    
    session = MagicMock()
    session.session_id = "test_session_123"
    session.user_id = "user123"
    session.status = SessionStatus.ACTIVE
    session.event_count = 0
    session.created_at = datetime.now(timezone.utc)
    session.last_active_at = datetime.now(timezone.utc)
    session.metadata = {"test": "data"}
    return session


@pytest.fixture
def auth_headers():
    """Mock authentication headers."""
    with patch("api.dependencies.decode_token") as mock_decode, \
         patch("api.dependencies.UserManager") as mock_user_manager_class:
        
        # Mock JWT decode
        mock_decode.return_value = {"sub": "user123", "username": "testuser", "type": "access"}
        
        # Mock UserManager
        mock_user_manager = MagicMock()
        mock_user = {"user_id": "user123", "username": "testuser"}
        mock_user_manager.get_user_by_id.return_value = mock_user
        mock_user_manager_class.return_value = mock_user_manager
        
        yield {"Authorization": "Bearer fake_token"}


class TestCreateSession:
    """Test session creation."""

    def test_create_session_success(self, client, auth_headers, mock_session):
        """Test successful session creation."""
        with patch("api.routers.sessions.SessionManager") as mock_manager_class:
            mock_manager = MagicMock()
            mock_manager.create_session.return_value = mock_session
            mock_manager_class.return_value = mock_manager

            response = client.post(
                "/sessions",
                json={"metadata": {"test": "data"}},
                headers=auth_headers,
            )

        assert response.status_code == 201
        data = response.json()
        assert data["session_id"] == "test_session_123"
        assert data["user_id"] == "user123"
        assert data["status"] == "active"


class TestListSessions:
    """Test listing sessions."""

    def test_list_sessions_success(self, client, auth_headers, mock_session):
        """Test successful session listing."""
        with patch("api.routers.sessions.SessionManager") as mock_manager_class:
            mock_manager = MagicMock()
            mock_manager.list_sessions.return_value = [mock_session]
            mock_manager_class.return_value = mock_manager

            response = client.get("/sessions", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert len(data["sessions"]) == 1
        assert data["sessions"][0]["session_id"] == "test_session_123"


class TestGetSession:
    """Test getting a session."""

    def test_get_session_success(self, client, auth_headers, mock_session):
        """Test successful session retrieval."""
        with patch("api.routers.sessions.SessionManager") as mock_manager_class:
            mock_manager = MagicMock()
            mock_manager.get_session.return_value = mock_session
            mock_manager_class.return_value = mock_manager

            response = client.get("/sessions/test_session_123", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert data["session_id"] == "test_session_123"

    def test_get_session_not_found(self, client, auth_headers):
        """Test session not found."""
        with patch("api.routers.sessions.SessionManager") as mock_manager_class:
            mock_manager = MagicMock()
            mock_manager.get_session.return_value = None
            mock_manager_class.return_value = mock_manager

            response = client.get("/sessions/nonexistent", headers=auth_headers)

        assert response.status_code == 404

    def test_get_session_unauthorized(self, client, auth_headers, mock_session):
        """Test unauthorized session access."""
        mock_session.user_id = "other_user"
        
        with patch("api.routers.sessions.SessionManager") as mock_manager_class:
            mock_manager = MagicMock()
            mock_manager.get_session.return_value = mock_session
            mock_manager_class.return_value = mock_manager

            response = client.get("/sessions/test_session_123", headers=auth_headers)

        assert response.status_code == 403


class TestCloseSession:
    """Test closing a session."""

    def test_close_session_success(self, client, auth_headers, mock_session):
        """Test successful session closure."""
        with patch("api.routers.sessions.SessionManager") as mock_manager_class:
            mock_manager = MagicMock()
            mock_manager.get_session.return_value = mock_session
            mock_manager_class.return_value = mock_manager

            response = client.delete("/sessions/test_session_123", headers=auth_headers)

        assert response.status_code == 204
