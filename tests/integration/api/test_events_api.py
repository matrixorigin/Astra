"""Integration tests for events API."""

from datetime import datetime, timezone
from unittest.mock import MagicMock, patch

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client():
    """Create test client."""
    from api.main import app
    return TestClient(app)


@pytest.fixture
def mock_event():
    """Mock event object."""
    event = MagicMock()
    event.event_id = "event_123"
    event.session_id = "session_123"
    event.user_id = "user123"
    event.event_type = "user_query"
    event.content = "Test query"
    event.created_at = datetime.now(timezone.utc)
    event.metadata = {}
    event.parent_event_id = None
    event.causal_chain_id = "chain_123"
    return event


@pytest.fixture
def mock_session():
    """Mock session object."""
    session = MagicMock()
    session.session_id = "session_123"
    session.user_id = "user123"
    return session


@pytest.fixture
def auth_headers():
    """Mock authentication headers."""
    with patch("api.dependencies.decode_token") as mock_decode, \
         patch("api.dependencies.UserManager") as mock_user_manager_class:
        
        mock_decode.return_value = {"sub": "user123", "username": "testuser", "type": "access"}
        
        mock_user_manager = MagicMock()
        mock_user = {"user_id": "user123", "username": "testuser"}
        mock_user_manager.get_user_by_id.return_value = mock_user
        mock_user_manager_class.return_value = mock_user_manager
        
        yield {"Authorization": "Bearer fake_token"}


class TestCreateEvent:
    """Test event creation."""

    def test_create_user_query_event(self, client, auth_headers, mock_event, mock_session):
        """Test creating a user query event."""
        with patch("api.routers.events.EventLogger") as mock_logger_class, \
             patch("api.routers.events.SessionManager") as mock_session_manager_class:
            
            mock_logger = MagicMock()
            mock_logger.create_user_query.return_value = mock_event
            mock_logger_class.return_value = mock_logger
            
            mock_session_manager = MagicMock()
            mock_session_manager.get_session.return_value = mock_session
            mock_session_manager_class.return_value = mock_session_manager

            response = client.post(
                "/events",
                json={
                    "session_id": "session_123",
                    "event_type": "user_query",
                    "content": "Test query",
                },
                headers=auth_headers,
            )

        assert response.status_code == 201
        data = response.json()
        assert data["event_id"] == "event_123"
        assert data["event_type"] == "user_query"

    def test_create_event_session_not_found(self, client, auth_headers):
        """Test creating event for non-existent session."""
        with patch("api.routers.events.SessionManager") as mock_session_manager_class:
            mock_session_manager = MagicMock()
            mock_session_manager.get_session.return_value = None
            mock_session_manager_class.return_value = mock_session_manager

            response = client.post(
                "/events",
                json={
                    "session_id": "nonexistent",
                    "event_type": "user_query",
                    "content": "Test",
                },
                headers=auth_headers,
            )

        assert response.status_code == 404

    def test_create_event_unauthorized(self, client, auth_headers, mock_session):
        """Test creating event for unauthorized session."""
        mock_session.user_id = "other_user"
        
        with patch("api.routers.events.SessionManager") as mock_session_manager_class:
            mock_session_manager = MagicMock()
            mock_session_manager.get_session.return_value = mock_session
            mock_session_manager_class.return_value = mock_session_manager

            response = client.post(
                "/events",
                json={
                    "session_id": "session_123",
                    "event_type": "user_query",
                    "content": "Test",
                },
                headers=auth_headers,
            )

        assert response.status_code == 403


class TestListEvents:
    """Test listing events."""

    def test_list_events_success(self, client, auth_headers, mock_event, mock_session):
        """Test successful event listing."""
        with patch("api.routers.events.EventLogger") as mock_logger_class, \
             patch("api.routers.events.SessionManager") as mock_session_manager_class:
            
            mock_logger = MagicMock()
            mock_logger.get_session_events.return_value = [mock_event]
            mock_logger_class.return_value = mock_logger
            
            mock_session_manager = MagicMock()
            mock_session_manager.get_session.return_value = mock_session
            mock_session_manager_class.return_value = mock_session_manager

            response = client.get("/events?session_id=session_123", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert len(data["events"]) == 1
        assert data["events"][0]["event_id"] == "event_123"


class TestGetEvent:
    """Test getting an event."""

    def test_get_event_success(self, client, auth_headers, mock_event):
        """Test successful event retrieval."""
        with patch("api.routers.events.EventLogger") as mock_logger_class:
            mock_logger = MagicMock()
            mock_logger.get_event.return_value = mock_event
            mock_logger_class.return_value = mock_logger

            response = client.get("/events/event_123", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert data["event_id"] == "event_123"

    def test_get_event_not_found(self, client, auth_headers):
        """Test event not found."""
        with patch("api.routers.events.EventLogger") as mock_logger_class:
            mock_logger = MagicMock()
            mock_logger.get_event.return_value = None
            mock_logger_class.return_value = mock_logger

            response = client.get("/events/nonexistent", headers=auth_headers)

        assert response.status_code == 404
