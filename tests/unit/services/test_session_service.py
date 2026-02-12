"""Unit tests for SessionService."""

import pytest
from unittest.mock import Mock, patch
from sqlalchemy.orm import Session

from api.services.session_service import SessionService
from api.models import Session as SessionModel


@pytest.fixture
def mock_db_session():
    """Mock database session."""
    return Mock(spec=Session)


@pytest.fixture
def session_service(mock_db_session):
    """Create SessionService with mocked dependencies."""
    with patch('api.services.session_service.SessionRepository') as mock_repo_class, \
         patch('api.services.session_service.Database') as mock_db_class, \
         patch('api.services.session_service.AuditLogger') as mock_audit_class:
        
        service = SessionService(mock_db_session)
        service.session_repo = Mock()
        service.audit = Mock()
        return service


class TestSessionService:
    """Test SessionService methods."""

    def test_create_session_success(self, session_service):
        """Test successful session creation."""
        user_id = "user123"
        agent_id = "agent123"
        title = "Test Session"
        metadata = {"client": "web"}
        
        mock_session = Mock(spec=SessionModel)
        mock_session.session_id = "session123"
        mock_session.user_id = user_id
        mock_session.agent_id = agent_id
        mock_session.title = title
        mock_session.status = "active"
        mock_session.event_count = 0
        mock_session.session_metadata = metadata
        mock_session.created_at = Mock()
        mock_session.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.updated_at = None
        mock_session.last_active_at = Mock()
        mock_session.last_active_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.ended_at = None
        
        session_service.session_repo.create.return_value = mock_session
        
        result = session_service.create_session(user_id, agent_id, title, metadata)
        
        assert result["session_id"] == "session123"
        assert result["user_id"] == user_id
        assert result["agent_id"] == agent_id
        assert result["title"] == title
        assert result["status"] == "active"
        assert result["metadata"] == metadata
        
        session_service.audit.log.assert_called_once()
        audit_call = session_service.audit.log.call_args
        assert audit_call[1]["action"] == "session_create"
        assert audit_call[1]["status"] == "success"

    def test_create_session_default_values(self, session_service):
        """Test session creation with default values."""
        user_id = "user123"
        
        mock_session = Mock(spec=SessionModel)
        mock_session.session_id = "session123"
        mock_session.user_id = user_id
        mock_session.agent_id = None
        mock_session.title = None
        mock_session.status = "active"
        mock_session.event_count = 0
        mock_session.session_metadata = {}
        mock_session.created_at = Mock()
        mock_session.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.updated_at = None
        mock_session.last_active_at = Mock()
        mock_session.last_active_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.ended_at = None
        
        session_service.session_repo.create.return_value = mock_session
        
        result = session_service.create_session(user_id)
        
        assert result["agent_id"] is None
        assert result["title"] is None
        assert result["metadata"] == {}

    def test_get_session_success(self, session_service):
        """Test successful session retrieval."""
        session_id = "session123"
        user_id = "user123"
        
        mock_session = Mock(spec=SessionModel)
        mock_session.session_id = session_id
        mock_session.user_id = user_id
        mock_session.agent_id = None
        mock_session.title = "Test Session"
        mock_session.status = "active"
        mock_session.event_count = 5
        mock_session.session_metadata = {}
        mock_session.created_at = Mock()
        mock_session.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.updated_at = None
        mock_session.last_active_at = Mock()
        mock_session.last_active_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.ended_at = None
        
        session_service.session_repo.get_by_id.return_value = mock_session
        
        result = session_service.get_session(session_id, user_id)
        
        assert result["session_id"] == session_id
        assert result["event_count"] == 5
        session_service.session_repo.get_by_id.assert_called_once_with(session_id)

    def test_get_session_not_found(self, session_service):
        """Test session retrieval when session not found."""
        session_service.session_repo.get_by_id.return_value = None
        
        with pytest.raises(ValueError, match="Session session123 不存在"):
            session_service.get_session("session123", "user123")

    def test_get_session_permission_denied(self, session_service):
        """Test session retrieval with permission denied."""
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = "other_user"
        session_service.session_repo.get_by_id.return_value = mock_session
        
        with pytest.raises(ValueError, match="无权限访问 Session session123"):
            session_service.get_session("session123", "user123")

    def test_list_sessions_success(self, session_service):
        """Test successful session listing."""
        user_id = "user123"
        
        mock_session = Mock(spec=SessionModel)
        mock_session.session_id = "session123"
        mock_session.user_id = user_id
        mock_session.agent_id = None
        mock_session.title = "Test Session"
        mock_session.status = "active"
        mock_session.event_count = 0
        mock_session.session_metadata = {}
        mock_session.created_at = Mock()
        mock_session.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.updated_at = None
        mock_session.last_active_at = Mock()
        mock_session.last_active_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_session.ended_at = None
        
        session_service.session_repo.list_by_user.return_value = ([mock_session], 1)
        
        result = session_service.list_sessions(user_id)
        
        assert result["sessions"][0]["session_id"] == "session123"
        assert result["total"] == 1
        session_service.session_repo.list_by_user.assert_called_once_with(
            user_id=user_id, agent_id=None, status=None, limit=50, offset=0
        )

    def test_list_sessions_with_filters(self, session_service):
        """Test session listing with filters."""
        user_id = "user123"
        agent_id = "agent123"
        status = "active"
        
        session_service.session_repo.list_by_user.return_value = ([], 0)
        
        session_service.list_sessions(user_id, agent_id=agent_id, status=status, limit=10, offset=5)
        
        session_service.session_repo.list_by_user.assert_called_once_with(
            user_id=user_id, agent_id=agent_id, status=status, limit=10, offset=5
        )

    def test_update_session_success(self, session_service):
        """Test successful session update."""
        session_id = "session123"
        user_id = "user123"
        new_title = "Updated Session"
        
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = user_id
        session_service.session_repo.get_by_id.return_value = mock_session
        
        updated_session = Mock(spec=SessionModel)
        updated_session.session_id = session_id
        updated_session.user_id = user_id
        updated_session.agent_id = None
        updated_session.title = new_title
        updated_session.status = "active"
        updated_session.event_count = 0
        updated_session.session_metadata = {}
        updated_session.created_at = Mock()
        updated_session.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        updated_session.updated_at = Mock()
        updated_session.updated_at.isoformat.return_value = "2023-01-01T01:00:00"
        updated_session.last_active_at = Mock()
        updated_session.last_active_at.isoformat.return_value = "2023-01-01T01:00:00"
        updated_session.ended_at = None
        
        session_service.session_repo.update.return_value = updated_session
        
        result = session_service.update_session(session_id, user_id, title=new_title)
        
        assert result["title"] == new_title
        session_service.session_repo.update.assert_called_once_with(session_id, {"title": new_title})
        session_service.audit.log.assert_called_once()

    def test_update_session_not_found(self, session_service):
        """Test session update when session not found."""
        session_service.session_repo.get_by_id.return_value = None
        
        with pytest.raises(ValueError, match="Session session123 不存在"):
            session_service.update_session("session123", "user123", title="New Title")

    def test_update_session_permission_denied(self, session_service):
        """Test session update with permission denied."""
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = "other_user"
        session_service.session_repo.get_by_id.return_value = mock_session
        
        with pytest.raises(ValueError, match="无权限修改 Session session123"):
            session_service.update_session("session123", "user123", title="New Title")

    def test_delete_session_success(self, session_service):
        """Test successful session deletion."""
        session_id = "session123"
        user_id = "user123"
        
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = user_id
        mock_session.title = "Test Session"
        session_service.session_repo.get_by_id.return_value = mock_session
        
        session_service.delete_session(session_id, user_id)
        
        session_service.session_repo.delete.assert_called_once_with(session_id)
        session_service.audit.log.assert_called_once()
        audit_call = session_service.audit.log.call_args
        assert audit_call[1]["action"] == "session_delete"
        assert audit_call[1]["status"] == "success"

    def test_delete_session_not_found(self, session_service):
        """Test session deletion when session not found."""
        session_service.session_repo.get_by_id.return_value = None
        
        with pytest.raises(ValueError, match="Session session123 不存在"):
            session_service.delete_session("session123", "user123")

    def test_delete_session_permission_denied(self, session_service):
        """Test session deletion with permission denied."""
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = "other_user"
        session_service.session_repo.get_by_id.return_value = mock_session
        
        with pytest.raises(ValueError, match="无权限删除 Session session123"):
            session_service.delete_session("session123", "user123")

    def test_create_session_exception_handling(self, session_service):
        """Test exception handling in create_session."""
        session_service.session_repo.create.side_effect = Exception("Database error")
        
        with pytest.raises(Exception):
            session_service.create_session("user123")
        
        # Verify audit log for failure
        session_service.audit.log.assert_called_once()
        audit_call = session_service.audit.log.call_args
        assert audit_call[1]["status"] == "failed"

    def test_update_session_exception_handling(self, session_service):
        """Test exception handling in update_session."""
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = "user123"
        session_service.session_repo.get_by_id.return_value = mock_session
        session_service.session_repo.update.side_effect = Exception("Database error")
        
        with pytest.raises(Exception):
            session_service.update_session("session123", "user123", title="New Title")
        
        # Verify audit log for failure
        session_service.audit.log.assert_called_once()
        audit_call = session_service.audit.log.call_args
        assert audit_call[1]["status"] == "failed"

    def test_delete_session_exception_handling(self, session_service):
        """Test exception handling in delete_session."""
        mock_session = Mock(spec=SessionModel)
        mock_session.user_id = "user123"
        mock_session.title = "Test Session"
        session_service.session_repo.get_by_id.return_value = mock_session
        session_service.session_repo.delete.side_effect = Exception("Database error")
        
        with pytest.raises(Exception):
            session_service.delete_session("session123", "user123")
        
        # Verify audit log for failure
        session_service.audit.log.assert_called_once()
        audit_call = session_service.audit.log.call_args
        assert audit_call[1]["status"] == "failed"
