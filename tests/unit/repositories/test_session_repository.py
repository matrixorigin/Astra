"""Unit tests for SessionRepository."""

import pytest
from unittest.mock import Mock, MagicMock, patch
from sqlalchemy.orm import Session, Query

from api.repositories.session_repository import SessionRepository
from api.models import Session as SessionModel


@pytest.fixture
def mock_db_session():
    """Mock database session."""
    return Mock(spec=Session)


@pytest.fixture
def session_repo(mock_db_session):
    """Create SessionRepository with mocked session."""
    return SessionRepository(mock_db_session)


class TestSessionRepository:
    """Test SessionRepository methods."""

    def test_create_success(self, session_repo, mock_db_session):
        """Test successful session creation."""
        session_data = {
            "session_id": "session123",
            "user_id": "user123",
            "status": "active",
            "event_count": 0
        }
        
        mock_session = Mock(spec=SessionModel)
        mock_db_session.add.return_value = None
        mock_db_session.commit.return_value = None
        mock_db_session.refresh.return_value = None
        
        with patch('api.repositories.session_repository.SessionModel', return_value=mock_session):
            result = session_repo.create(session_data)
            
            assert result == mock_session
            mock_db_session.add.assert_called_once_with(mock_session)
            mock_db_session.commit.assert_called_once()
            mock_db_session.refresh.assert_called_once_with(mock_session)

    def test_get_by_id_success(self, session_repo, mock_db_session):
        """Test successful session retrieval by ID."""
        session_id = "session123"
        mock_session = Mock(spec=SessionModel)
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = mock_session
        mock_db_session.query.return_value = mock_query
        
        result = session_repo.get_by_id(session_id)
        
        assert result == mock_session
        mock_db_session.query.assert_called_once_with(SessionModel)
        mock_query.filter.assert_called_once()
        mock_query.first.assert_called_once()

    def test_get_by_id_not_found(self, session_repo, mock_db_session):
        """Test session retrieval when not found."""
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = None
        mock_db_session.query.return_value = mock_query
        
        result = session_repo.get_by_id("nonexistent")
        
        assert result is None

    def test_list_by_user_success(self, session_repo, mock_db_session):
        """Test successful session listing by user."""
        user_id = "user123"
        mock_sessions = [Mock(spec=SessionModel), Mock(spec=SessionModel)]
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.count.return_value = 2
        mock_query.order_by.return_value = mock_query
        mock_query.offset.return_value = mock_query
        mock_query.limit.return_value = mock_query
        mock_query.all.return_value = mock_sessions
        mock_db_session.query.return_value = mock_query
        
        sessions, total = session_repo.list_by_user(user_id)
        
        assert sessions == mock_sessions
        assert total == 2
        mock_db_session.query.assert_called_with(SessionModel)
        mock_query.filter.assert_called()
        mock_query.offset.assert_called_once_with(0)
        mock_query.limit.assert_called_once_with(50)

    def test_list_by_user_with_filters(self, session_repo, mock_db_session):
        """Test session listing with filters."""
        user_id = "user123"
        agent_id = "agent123"
        status = "active"
        limit = 10
        offset = 5
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.count.return_value = 0
        mock_query.order_by.return_value = mock_query
        mock_query.offset.return_value = mock_query
        mock_query.limit.return_value = mock_query
        mock_query.all.return_value = []
        mock_db_session.query.return_value = mock_query
        
        sessions, total = session_repo.list_by_user(
            user_id, agent_id=agent_id, status=status, 
            limit=limit, offset=offset
        )
        
        assert sessions == []
        assert total == 0
        # Should be called multiple times for different filters
        assert mock_query.filter.call_count >= 2
        mock_query.offset.assert_called_once_with(offset)
        mock_query.limit.assert_called_once_with(limit)

    def test_update_success(self, session_repo, mock_db_session):
        """Test successful session update."""
        session_id = "session123"
        updates = {"title": "Updated Session", "status": "closed"}
        
        mock_session = Mock(spec=SessionModel)
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = mock_session
        mock_db_session.query.return_value = mock_query
        
        result = session_repo.update(session_id, updates)
        
        assert result == mock_session
        mock_db_session.commit.assert_called_once()
        mock_db_session.refresh.assert_called_once_with(mock_session)

    def test_update_not_found(self, session_repo, mock_db_session):
        """Test session update when session not found."""
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = None
        mock_db_session.query.return_value = mock_query
        
        result = session_repo.update("session123", {"title": "New Title"})
        
        assert result is None
        mock_db_session.commit.assert_not_called()

    def test_delete_success(self, session_repo, mock_db_session):
        """Test successful session deletion."""
        session_id = "session123"
        mock_session = Mock(spec=SessionModel)
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = mock_session
        mock_db_session.query.return_value = mock_query
        
        result = session_repo.delete(session_id)
        
        assert result is True
        mock_db_session.delete.assert_called_once_with(mock_session)
        mock_db_session.commit.assert_called_once()

    def test_delete_not_found(self, session_repo, mock_db_session):
        """Test session deletion when session not found."""
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = None
        mock_db_session.query.return_value = mock_query
        
        result = session_repo.delete("session123")
        
        assert result is False
        mock_db_session.delete.assert_not_called()
        mock_db_session.commit.assert_not_called()
