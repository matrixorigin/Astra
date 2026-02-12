"""Sandbox Service 单元测试"""

import pytest
from unittest.mock import Mock, MagicMock, patch
from datetime import datetime, timezone

from api.services.sandbox_service import SandboxService


@pytest.fixture
def mock_db_session():
    """Mock SQLAlchemy session"""
    return Mock()


@pytest.fixture
def mock_sandbox():
    """Mock Sandbox"""
    with patch('api.services.sandbox_service.Sandbox') as mock:
        yield mock.return_value


@pytest.fixture
def mock_audit():
    """Mock AuditLogger"""
    with patch('api.services.sandbox_service.AuditLogger') as mock:
        yield mock.return_value


@pytest.fixture
def mock_permission():
    """Mock PermissionChecker"""
    with patch('api.services.sandbox_service.PermissionChecker') as mock:
        yield mock.return_value


@pytest.fixture
def service(mock_db_session, mock_sandbox, mock_audit, mock_permission):
    """SandboxService instance with mocks"""
    with patch('api.services.sandbox_service.Database'):
        service = SandboxService(mock_db_session)
        service.sandbox = mock_sandbox
        service.audit = mock_audit
        service.permission = mock_permission
        return service


class TestCreateSandbox:
    """测试创建 sandbox"""
    
    def test_create_sandbox_success(self, service, mock_permission, mock_sandbox, mock_audit):
        """测试成功创建 sandbox"""
        # Setup
        mock_permission.has_role.return_value = True
        
        # Execute
        result = service.create_sandbox(
            name="test_sandbox",
            user_id="user123",
            description="Test sandbox"
        )
        
        # Verify
        assert result["sandbox_name"] == "test_sandbox"
        assert result["description"] == "Test sandbox"
        assert result["created_by"] == "user123"
        
        mock_sandbox.create.assert_called_once_with(
            name="test_sandbox",
            description="Test sandbox",
            created_by="user123"
        )
        mock_audit.log.assert_called_once()
        assert mock_audit.log.call_args[1]["status"] == "success"
    
    def test_create_sandbox_empty_name(self, service, mock_permission):
        """测试空名称"""
        # Setup
        mock_permission.has_role.return_value = True
        
        # Execute & Verify
        with pytest.raises(ValueError, match="不能为空"):
            service.create_sandbox(
                name="",
                user_id="user123"
            )
    
    def test_create_sandbox_failure_audit(self, service, mock_permission, mock_sandbox, mock_audit):
        """测试创建失败时记录审计日志"""
        # Setup
        mock_permission.has_role.return_value = True
        mock_sandbox.create.side_effect = Exception("Database error")
        
        # Execute & Verify
        with pytest.raises(Exception, match="Database error"):
            service.create_sandbox(
                name="test_sandbox",
                user_id="user123"
            )
        
        # 验证失败也记录了审计日志
        assert mock_audit.log.call_count == 1
        assert mock_audit.log.call_args[1]["status"] == "failed"


class TestListSandboxes:
    """测试列出 sandboxes"""
    
    def test_list_sandboxes_all(self, service, mock_permission, mock_sandbox):
        """测试列出所有 sandboxes (开发模式)"""
        # Setup
        mock_permission.has_role.return_value = True
        mock_sandbox.list_sandboxes.return_value = [
            {"sandbox_name": "sandbox1", "created_by": "user1"},
            {"sandbox_name": "sandbox2", "created_by": "user2"},
        ]
        
        # Execute
        result = service.list_sandboxes(user_id="user1", pattern="%")
        
        # Verify - 开发模式返回所有
        assert len(result) == 2
        mock_sandbox.list_sandboxes.assert_called_once_with(pattern="%")


class TestDeleteSandbox:
    """测试删除 sandbox"""
    
    def test_delete_sandbox_success(self, service, mock_permission, mock_sandbox, mock_audit):
        """测试删除 sandbox (开发模式)"""
        # Setup
        mock_permission.has_role.return_value = True
        mock_sandbox.list_sandboxes.return_value = [
            {"sandbox_name": "test_sandbox", "created_by": "user123"}
        ]
        
        # Execute
        service.delete_sandbox(name="test_sandbox", user_id="user123")
        
        # Verify
        mock_sandbox.delete.assert_called_once_with("test_sandbox")
        mock_audit.log.assert_called_once()
        assert mock_audit.log.call_args[1]["status"] == "success"
    
    def test_delete_sandbox_not_found(self, service, mock_permission, mock_sandbox):
        """测试删除不存在的 sandbox"""
        # Setup
        mock_permission.has_role.return_value = True
        mock_sandbox.list_sandboxes.return_value = []
        
        # Execute & Verify
        with pytest.raises(ValueError, match="不存在"):
            service.delete_sandbox(name="nonexistent", user_id="user123")


class TestGetSandboxInfo:
    """测试获取 sandbox 信息"""
    
    def test_get_sandbox_info_success(self, service, mock_permission, mock_sandbox):
        """测试获取信息 (开发模式)"""
        # Setup
        mock_permission.has_role.return_value = True
        mock_sandbox.list_sandboxes.return_value = [
            {"sandbox_name": "test_sandbox", "created_by": "user123", "description": "Test"}
        ]
        
        # Execute
        result = service.get_sandbox_info(name="test_sandbox", user_id="user123")
        
        # Verify
        assert result["sandbox_name"] == "test_sandbox"
        assert result["created_by"] == "user123"
    
    def test_get_sandbox_info_not_found(self, service, mock_permission, mock_sandbox):
        """测试获取不存在的 sandbox"""
        # Setup
        mock_permission.has_role.return_value = True
        mock_sandbox.list_sandboxes.return_value = []
        
        # Execute & Verify
        with pytest.raises(ValueError, match="不存在"):
            service.get_sandbox_info(name="nonexistent", user_id="user123")
