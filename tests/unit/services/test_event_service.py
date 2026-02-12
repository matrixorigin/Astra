"""Simplified unit tests for EventService."""

import pytest
from unittest.mock import Mock, patch
from sqlalchemy.orm import Session

from api.services.event_service import EventService
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from api.models import Event as EventModel, Session as SessionModel


@pytest.fixture
def mock_db_session():
    """Mock database session."""
    return Mock(spec=Session)


@pytest.fixture
def event_service(mock_db_session):
    """Create EventService with mocked dependencies."""
    with patch('api.services.event_service.EventRepository') as mock_event_repo_class, \
         patch('api.services.event_service.SessionRepository') as mock_session_repo_class, \
         patch('api.services.event_service.Database') as mock_db_class, \
         patch('api.services.event_service.AuditLogger') as mock_audit_class:
        
        service = EventService(mock_db_session)
        service.event_repo = Mock()
        service.session_repo = Mock()
        service.audit = Mock()
        return service


class TestEventService:
    """Test EventService methods."""

    def test_create_event_success(self, event_service):
        """Test successful event creation."""
        user_id = "user123"
        session_id = "session123"
        event_type = "user_query"
        content = "Hello, world!"
        metadata = {"client": "web"}
        
        # Mock session exists and belongs to user
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = user_id
        event_service.session_repo.get_by_id.return_value = mock_session
        
        mock_event = Mock(spec=EventModel)
        mock_event.event_id = "event123"
        mock_event.session_id = session_id
        mock_event.user_id = user_id
        mock_event.event_type = event_type
        mock_event.content = content
        mock_event.event_metadata = metadata
        mock_event.agent_id = None
        mock_event.agent_version = None
        mock_event.parent_event_id = None
        mock_event.causal_chain_id = "chain123"
        mock_event.created_at = Mock()
        mock_event.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        
        event_service.event_repo.create.return_value = mock_event
        
        result = event_service.create_event(
            user_id=user_id,
            session_id=session_id,
            event_type=event_type,
            content=content,
            metadata=metadata
        )
        
        assert result["event_id"] == "event123"
        assert result["session_id"] == session_id
        assert result["event_type"] == event_type
        assert result["content"] == content
        assert result["metadata"] == metadata
        
        event_service.audit.log.assert_called_once()
        audit_call = event_service.audit.log.call_args
        assert audit_call[1]["action"] == "event_create"
        assert audit_call[1]["status"] == "success"

    def test_create_event_session_not_found(self, event_service):
        """Test event creation when session not found."""
        event_service.session_repo.get_by_id.return_value = None
        
        with pytest.raises(ResourceNotFoundError, match="Session session123 不存在"):
            event_service.create_event(
                user_id="user123",
                session_id="session123",
                event_type="user_query",
                content="Hello"
            )

    def test_get_event_success(self, event_service):
        """Test successful event retrieval."""
        event_id = "event123"
        user_id = "user123"
        
        mock_event = Mock(spec=EventModel)
        mock_event.event_id = event_id
        mock_event.session_id = "session123"
        mock_event.user_id = user_id
        mock_event.event_type = "user_query"
        mock_event.content = "Hello"
        mock_event.event_metadata = {}
        mock_event.agent_id = None
        mock_event.agent_version = None
        mock_event.parent_event_id = None
        mock_event.causal_chain_id = "chain123"
        mock_event.created_at = Mock()
        mock_event.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        
        event_service.event_repo.get_by_id.return_value = mock_event
        
        result = event_service.get_event(event_id, user_id)
        
        assert result["event_id"] == event_id
        assert result["user_id"] == user_id
        event_service.event_repo.get_by_id.assert_called_once_with(event_id)

    def test_get_event_not_found(self, event_service):
        """Test event retrieval when event not found."""
        event_service.event_repo.get_by_id.return_value = None
        
        with pytest.raises(ResourceNotFoundError, match="Event event123 不存在"):
            event_service.get_event("event123", "user123")

    def test_get_event_permission_denied(self, event_service):
        """Test event retrieval with permission denied."""
        mock_event = Mock(spec=EventModel)
        mock_event.user_id = "other_user"
        event_service.event_repo.get_by_id.return_value = mock_event
        
        with pytest.raises(PermissionDeniedError, match="无权限访问 Event event123"):
            event_service.get_event("event123", "user123")

    def test_list_events_success(self, event_service):
        """Test successful event listing."""
        user_id = "user123"
        
        mock_event = Mock(spec=EventModel)
        mock_event.event_id = "event123"
        mock_event.session_id = "session123"
        mock_event.user_id = user_id
        mock_event.event_type = "user_query"
        mock_event.content = "Hello"
        mock_event.event_metadata = {}
        mock_event.agent_id = None
        mock_event.agent_version = None
        mock_event.parent_event_id = None
        mock_event.causal_chain_id = "chain123"
        mock_event.created_at = Mock()
        mock_event.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        
        event_service.event_repo.get_by_user.return_value = ([mock_event], 1)
        
        result = event_service.list_events(user_id)
        
        assert result["events"][0]["event_id"] == "event123"
        assert result["total"] == 1
        event_service.event_repo.get_by_user.assert_called_once()

    def test_create_event_exception_handling(self, event_service):
        """Test exception handling in create_event."""
        # Mock session exists
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = "user123"
        event_service.session_repo.get_by_id.return_value = mock_session
        
        event_service.event_repo.create.side_effect = Exception("Database error")
        
        with pytest.raises(Exception):
            event_service.create_event(
                user_id="user123",
                session_id="session123",
                event_type="user_query",
                content="Hello"
            )
        
        # Verify audit log for failure
        event_service.audit.log.assert_called_once()
        audit_call = event_service.audit.log.call_args
        assert audit_call[1]["status"] == "failed"
