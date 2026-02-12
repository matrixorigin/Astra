"""Unit tests for AgentService."""

import pytest
from unittest.mock import Mock, patch
from sqlalchemy.orm import Session

from api.services.agent_service import AgentService
from api.models import Agent as AgentModel


@pytest.fixture
def mock_db_session():
    """Mock database session."""
    return Mock(spec=Session)


@pytest.fixture
def mock_agent_repo():
    """Mock agent repository."""
    return Mock()


@pytest.fixture
def mock_audit():
    """Mock audit logger."""
    return Mock()


@pytest.fixture
def mock_permission():
    """Mock permission checker."""
    return Mock()


@pytest.fixture
def agent_service(mock_db_session):
    """Create AgentService with mocked dependencies."""
    with patch('api.services.agent_service.AgentRepository') as mock_repo_class, \
         patch('api.services.agent_service.Database') as mock_db_class, \
         patch('api.services.agent_service.AuditLogger') as mock_audit_class, \
         patch('api.services.agent_service.PermissionChecker') as mock_perm_class:
        
        service = AgentService(mock_db_session)
        service.agent_repo = Mock()
        service.audit = Mock()
        service.permission = Mock()
        return service


class TestAgentService:
    """Test AgentService methods."""

    def test_create_agent_success(self, agent_service):
        """Test successful agent creation."""
        # Setup
        user_id = "user123"
        name = "Test Agent"
        agent_config = {"model": "gpt-4"}
        data_source = {"type": "matrixone"}
        
        mock_agent = Mock(spec=AgentModel)
        mock_agent.agent_id = "agent123"
        mock_agent.agent_name = name
        mock_agent.agent_type = "general"
        mock_agent.owner_user_id = user_id
        mock_agent.agent_config = agent_config
        mock_agent.data_source = data_source
        mock_agent.is_active = True
        mock_agent.created_at = Mock()
        mock_agent.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_agent.updated_at = None
        
        agent_service.agent_repo.create.return_value = mock_agent
        
        # Execute
        result = agent_service.create_agent(user_id, name, agent_config, data_source)
        
        # Verify
        assert result["agent_id"] == "agent123"
        assert result["name"] == name
        assert result["agent_type"] == "general"
        assert result["owner_user_id"] == user_id
        assert result["agent_config"] == agent_config
        assert result["data_source"] == data_source
        assert result["is_active"] is True
        
        # Verify audit log
        agent_service.audit.log.assert_called_once()
        audit_call = agent_service.audit.log.call_args
        assert audit_call[1]["action"] == "agent_create"
        assert audit_call[1]["status"] == "success"

    def test_create_agent_empty_name(self, agent_service):
        """Test agent creation with empty name."""
        with pytest.raises(ValueError, match="Agent name 不能为空"):
            agent_service.create_agent("user123", "")

    def test_create_agent_whitespace_name(self, agent_service):
        """Test agent creation with whitespace-only name."""
        with pytest.raises(ValueError, match="Agent name 不能为空"):
            agent_service.create_agent("user123", "   ")

    def test_create_agent_default_values(self, agent_service):
        """Test agent creation with default values."""
        user_id = "user123"
        name = "Test Agent"
        
        mock_agent = Mock(spec=AgentModel)
        mock_agent.agent_id = "agent123"
        mock_agent.agent_name = name
        mock_agent.agent_type = "general"
        mock_agent.owner_user_id = user_id
        mock_agent.agent_config = {}
        mock_agent.data_source = {"type": "matrixone", "database": "dev_agent"}
        mock_agent.is_active = True
        mock_agent.created_at = Mock()
        mock_agent.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_agent.updated_at = None
        
        agent_service.agent_repo.create.return_value = mock_agent
        
        result = agent_service.create_agent(user_id, name)
        
        # Verify defaults
        assert result["agent_config"] == {}
        assert result["data_source"]["type"] == "matrixone"
        assert result["data_source"]["database"] == "dev_agent"

    def test_get_agent_success(self, agent_service):
        """Test successful agent retrieval."""
        agent_id = "agent123"
        user_id = "user123"
        
        mock_agent = Mock(spec=AgentModel)
        mock_agent.agent_id = agent_id
        mock_agent.agent_name = "Test Agent"
        mock_agent.agent_type = "general"
        mock_agent.owner_user_id = user_id
        mock_agent.agent_config = {}
        mock_agent.data_source = {}
        mock_agent.is_active = True
        mock_agent.created_at = Mock()
        mock_agent.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_agent.updated_at = None
        
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        
        result = agent_service.get_agent(agent_id, user_id)
        
        assert result["agent_id"] == agent_id
        assert result["name"] == "Test Agent"
        agent_service.agent_repo.get_by_id.assert_called_once_with(agent_id)

    def test_get_agent_not_found(self, agent_service):
        """Test agent retrieval when agent not found."""
        agent_service.agent_repo.get_by_id.return_value = None
        
        with pytest.raises(ValueError, match="Agent agent123 不存在"):
            agent_service.get_agent("agent123", "user123")

    def test_get_agent_permission_denied(self, agent_service):
        """Test agent retrieval with permission denied."""
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = "other_user"
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        
        with pytest.raises(ValueError, match="无权限访问 Agent agent123"):
            agent_service.get_agent("agent123", "user123")

    def test_list_agents_success(self, agent_service):
        """Test successful agent listing."""
        user_id = "user123"
        
        mock_agent = Mock(spec=AgentModel)
        mock_agent.agent_id = "agent123"
        mock_agent.agent_name = "Test Agent"
        mock_agent.agent_type = "general"
        mock_agent.owner_user_id = user_id
        mock_agent.agent_config = {}
        mock_agent.data_source = {}
        mock_agent.is_active = True
        mock_agent.created_at = Mock()
        mock_agent.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        mock_agent.updated_at = None
        
        agent_service.agent_repo.list_by_owner.return_value = [mock_agent]
        
        result = agent_service.list_agents(user_id)
        
        assert len(result) == 1
        assert result[0]["agent_id"] == "agent123"
        agent_service.agent_repo.list_by_owner.assert_called_once_with(user_id)

    def test_update_agent_success(self, agent_service):
        """Test successful agent update."""
        agent_id = "agent123"
        user_id = "user123"
        new_name = "Updated Agent"
        
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = user_id
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        
        updated_agent = Mock(spec=AgentModel)
        updated_agent.agent_id = agent_id
        updated_agent.agent_name = new_name
        updated_agent.agent_type = "general"
        updated_agent.owner_user_id = user_id
        updated_agent.agent_config = {}
        updated_agent.data_source = {}
        updated_agent.is_active = True
        updated_agent.created_at = Mock()
        updated_agent.created_at.isoformat.return_value = "2023-01-01T00:00:00"
        updated_agent.updated_at = Mock()
        updated_agent.updated_at.isoformat.return_value = "2023-01-01T01:00:00"
        
        agent_service.agent_repo.update.return_value = updated_agent
        
        result = agent_service.update_agent(agent_id, user_id, name=new_name)
        
        assert result["name"] == new_name
        agent_service.agent_repo.update.assert_called_once_with(agent_id, user_id, {"agent_name": new_name})
        agent_service.audit.log.assert_called_once()

    def test_update_agent_not_found(self, agent_service):
        """Test agent update when agent not found."""
        agent_service.agent_repo.get_by_id.return_value = None
        
        with pytest.raises(ValueError, match="Agent agent123 不存在"):
            agent_service.update_agent("agent123", "user123", name="New Name")

    def test_update_agent_permission_denied(self, agent_service):
        """Test agent update with permission denied."""
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = "other_user"
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        
        with pytest.raises(ValueError, match="无权限修改 Agent agent123"):
            agent_service.update_agent("agent123", "user123", name="New Name")

    def test_update_agent_no_changes(self, agent_service):
        """Test agent update with no changes."""
        agent_id = "agent123"
        user_id = "user123"
        
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = user_id
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        
        # Mock get_agent method
        agent_service.get_agent = Mock(return_value={"agent_id": agent_id})
        
        result = agent_service.update_agent(agent_id, user_id)
        
        # Should return current agent info without calling update
        assert result["agent_id"] == agent_id
        agent_service.agent_repo.update.assert_not_called()

    def test_delete_agent_success(self, agent_service):
        """Test successful agent deletion."""
        agent_id = "agent123"
        user_id = "user123"
        
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = user_id
        mock_agent.agent_name = "Test Agent"
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        
        agent_service.delete_agent(agent_id, user_id)
        
        agent_service.agent_repo.delete.assert_called_once_with(agent_id, user_id)
        agent_service.audit.log.assert_called_once()
        audit_call = agent_service.audit.log.call_args
        assert audit_call[1]["action"] == "agent_delete"
        assert audit_call[1]["status"] == "success"

    def test_delete_agent_not_found(self, agent_service):
        """Test agent deletion when agent not found."""
        agent_service.agent_repo.get_by_id.return_value = None
        
        with pytest.raises(ValueError, match="Agent agent123 不存在"):
            agent_service.delete_agent("agent123", "user123")

    def test_delete_agent_permission_denied(self, agent_service):
        """Test agent deletion with permission denied."""
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = "other_user"
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        
        with pytest.raises(ValueError, match="无权限删除 Agent agent123"):
            agent_service.delete_agent("agent123", "user123")

    def test_create_agent_exception_handling(self, agent_service):
        """Test exception handling in create_agent."""
        agent_service.agent_repo.create.side_effect = Exception("Database error")
        
        with pytest.raises(Exception):
            agent_service.create_agent("user123", "Test Agent")
        
        # Verify audit log for failure
        agent_service.audit.log.assert_called_once()
        audit_call = agent_service.audit.log.call_args
        assert audit_call[1]["status"] == "failed"

    def test_update_agent_exception_handling(self, agent_service):
        """Test exception handling in update_agent."""
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = "user123"
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        agent_service.agent_repo.update.side_effect = Exception("Database error")
        
        with pytest.raises(Exception):
            agent_service.update_agent("agent123", "user123", name="New Name")
        
        # Verify audit log for failure
        agent_service.audit.log.assert_called_once()
        audit_call = agent_service.audit.log.call_args
        assert audit_call[1]["status"] == "failed"

    def test_delete_agent_exception_handling(self, agent_service):
        """Test exception handling in delete_agent."""
        mock_agent = Mock(spec=AgentModel)
        mock_agent.owner_user_id = "user123"
        mock_agent.agent_name = "Test Agent"
        agent_service.agent_repo.get_by_id.return_value = mock_agent
        agent_service.agent_repo.delete.side_effect = Exception("Database error")
        
        with pytest.raises(Exception):
            agent_service.delete_agent("agent123", "user123")
        
        # Verify audit log for failure
        agent_service.audit.log.assert_called_once()
        audit_call = agent_service.audit.log.call_args
        assert audit_call[1]["status"] == "failed"
