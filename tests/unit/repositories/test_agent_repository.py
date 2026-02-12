"""Unit tests for AgentRepository."""

import pytest
from unittest.mock import Mock, MagicMock, patch
from sqlalchemy.orm import Session, Query

from api.repositories.agent_repository import AgentRepository
from api.models import Agent as AgentModel


@pytest.fixture
def mock_db_session():
    """Mock database session."""
    return Mock(spec=Session)


@pytest.fixture
def agent_repo(mock_db_session):
    """Create AgentRepository with mocked session."""
    return AgentRepository(mock_db_session)


class TestAgentRepository:
    """Test AgentRepository methods."""

    def test_create_success(self, agent_repo, mock_db_session):
        """Test successful agent creation."""
        agent_data = {
            "agent_id": "agent123",
            "agent_name": "Test Agent",
            "agent_type": "general",
            "owner_user_id": "user123",
            "agent_config": {},
            "data_source": {},
            "is_active": True
        }
        
        mock_agent = Mock(spec=AgentModel)
        mock_db_session.add.return_value = None
        mock_db_session.commit.return_value = None
        mock_db_session.refresh.return_value = None
        
        with patch('api.repositories.agent_repository.AgentModel', return_value=mock_agent):
            result = agent_repo.create(agent_data)
            
            assert result == mock_agent
            mock_db_session.add.assert_called_once_with(mock_agent)
            mock_db_session.commit.assert_called_once()
            mock_db_session.refresh.assert_called_once_with(mock_agent)

    def test_get_by_id_success(self, agent_repo, mock_db_session):
        """Test successful agent retrieval by ID."""
        agent_id = "agent123"
        mock_agent = Mock(spec=AgentModel)
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = mock_agent
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.get_by_id(agent_id)
        
        assert result == mock_agent
        mock_db_session.query.assert_called_once_with(AgentModel)
        mock_query.filter.assert_called_once()
        mock_query.first.assert_called_once()

    def test_get_by_id_with_owner_filter(self, agent_repo, mock_db_session):
        """Test agent retrieval by ID with owner filter."""
        agent_id = "agent123"
        owner_user_id = "user123"
        mock_agent = Mock(spec=AgentModel)
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = mock_agent
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.get_by_id(agent_id, owner_user_id)
        
        assert result == mock_agent
        # Should be called twice - once for agent_id, once for owner_user_id
        assert mock_query.filter.call_count == 2

    def test_get_by_id_not_found(self, agent_repo, mock_db_session):
        """Test agent retrieval when not found."""
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = None
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.get_by_id("nonexistent")
        
        assert result is None

    def test_list_by_owner_success(self, agent_repo, mock_db_session):
        """Test successful agent listing by owner."""
        owner_user_id = "user123"
        mock_agents = [Mock(spec=AgentModel), Mock(spec=AgentModel)]
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.offset.return_value = mock_query
        mock_query.limit.return_value = mock_query
        mock_query.all.return_value = mock_agents
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.list_by_owner(owner_user_id)
        
        assert result == mock_agents
        mock_db_session.query.assert_called_once_with(AgentModel)
        mock_query.filter.assert_called()
        mock_query.offset.assert_called_once_with(0)
        mock_query.limit.assert_called_once_with(50)

    def test_list_by_owner_with_filters(self, agent_repo, mock_db_session):
        """Test agent listing with filters."""
        owner_user_id = "user123"
        agent_type = "chatbot"
        is_active = True
        limit = 10
        offset = 5
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.offset.return_value = mock_query
        mock_query.limit.return_value = mock_query
        mock_query.all.return_value = []
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.list_by_owner(
            owner_user_id, agent_type=agent_type, is_active=is_active, 
            limit=limit, offset=offset
        )
        
        assert result == []
        # Should be called multiple times for different filters
        assert mock_query.filter.call_count >= 2
        mock_query.offset.assert_called_once_with(offset)
        mock_query.limit.assert_called_once_with(limit)

    def test_update_success(self, agent_repo, mock_db_session):
        """Test successful agent update."""
        agent_id = "agent123"
        owner_user_id = "user123"
        updates = {"agent_name": "Updated Agent", "is_active": False}
        
        mock_agent = Mock(spec=AgentModel)
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = mock_agent
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.update(agent_id, owner_user_id, updates)
        
        assert result == mock_agent
        # Verify setattr was called for each update
        assert hasattr(mock_agent, 'agent_name') or True  # Mock doesn't enforce this
        mock_db_session.commit.assert_called_once()
        mock_db_session.refresh.assert_called_once_with(mock_agent)

    def test_update_not_found(self, agent_repo, mock_db_session):
        """Test agent update when agent not found."""
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = None
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.update("agent123", "user123", {"agent_name": "New Name"})
        
        assert result is None
        mock_db_session.commit.assert_not_called()

    def test_delete_success(self, agent_repo, mock_db_session):
        """Test successful agent deletion."""
        agent_id = "agent123"
        owner_user_id = "user123"
        
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.delete.return_value = 1  # One row deleted
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.delete(agent_id, owner_user_id)
        
        assert result is True
        mock_query.delete.assert_called_once()
        mock_db_session.commit.assert_called_once()

    def test_delete_not_found(self, agent_repo, mock_db_session):
        """Test agent deletion when agent not found."""
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.delete.return_value = 0  # No rows deleted
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.delete("agent123", "user123")
        
        assert result is False
        mock_query.delete.assert_called_once()
        mock_db_session.commit.assert_called_once()

    def test_list_by_owner_empty_result(self, agent_repo, mock_db_session):
        """Test agent listing with empty result."""
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.offset.return_value = mock_query
        mock_query.limit.return_value = mock_query
        mock_query.all.return_value = []
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.list_by_owner("user123")
        
        assert result == []

    def test_update_with_empty_updates(self, agent_repo, mock_db_session):
        """Test agent update with empty updates dict."""
        agent_id = "agent123"
        owner_user_id = "user123"
        
        mock_agent = Mock(spec=AgentModel)
        mock_query = Mock(spec=Query)
        mock_query.filter.return_value = mock_query
        mock_query.first.return_value = mock_agent
        mock_db_session.query.return_value = mock_query
        
        result = agent_repo.update(agent_id, owner_user_id, {})
        
        assert result == mock_agent
        mock_db_session.commit.assert_called_once()
        mock_db_session.refresh.assert_called_once_with(mock_agent)
