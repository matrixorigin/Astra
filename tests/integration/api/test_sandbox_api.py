"""Sandbox API 集成测试

注意: 这些测试需要完整的MatrixOne环境和清理逻辑
当前状态: 部分测试通过，部分需要环境配置
"""

import pytest
from fastapi.testclient import TestClient
from uuid_utils import uuid7

from api.main import app
from api.database import get_db_session
from api.repositories import UserRepository
from core.auth.jwt_manager import create_access_token
from core.sandbox import Sandbox
from sdk import Database


pytestmark = pytest.mark.skip(reason="需要完整的MatrixOne环境和清理逻辑")


@pytest.fixture
def client():
    """Test client"""
    return TestClient(app)


@pytest.fixture
def db_session():
    """Database session"""
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def test_user(db_session):
    """Create test user"""
    user_repo = UserRepository(db_session)
    user = user_repo.create({
        "user_id": str(uuid7()),
        "username": f"test_user_{str(uuid7())[:8]}",
        "email": f"test_{str(uuid7())[:8]}@example.com",
        "password_hash": "hashed_password",
        "is_active": True
    })
    yield user
    # Cleanup
    try:
        db_session.delete(user)
        db_session.commit()
    except:
        pass


@pytest.fixture
def auth_token(test_user):
    """Generate auth token"""
    return create_access_token({"sub": test_user.user_id})


@pytest.fixture
def auth_headers(auth_token):
    """Auth headers"""
    return {"Authorization": f"Bearer {auth_token}"}


@pytest.fixture(autouse=True)
def cleanup_sandboxes():
    """Cleanup test sandboxes before and after"""
    # Cleanup before test
    db = Database()
    sandbox = Sandbox(db=db)
    sandboxes = sandbox.list_sandboxes(pattern="test_sandbox_%")
    for s in sandboxes:
        try:
            sandbox.delete(s["sandbox_name"])
        except:
            pass
    
    yield
    
    # Cleanup after test
    sandboxes = sandbox.list_sandboxes(pattern="test_sandbox_%")
    for s in sandboxes:
        try:
            sandbox.delete(s["sandbox_name"])
        except:
            pass


class TestCreateSandbox:
    """测试创建 sandbox API"""
    
    def test_create_sandbox_success(self, client, auth_headers):
        """测试成功创建 sandbox"""
        response = client.post(
            "/sandbox",
            json={
                "name": f"test_sandbox_{str(uuid7())[:8]}",
                "description": "Test sandbox"
            },
            headers=auth_headers
        )
        
        assert response.status_code == 201
        data = response.json()
        assert "sandbox_name" in data
        assert data["description"] == "Test sandbox"
    
    def test_create_sandbox_without_auth(self, client):
        """测试未认证"""
        response = client.post(
            "/sandbox",
            json={"name": "test_sandbox"}
        )
        
        assert response.status_code == 403  # HTTPBearer returns 403
    
    def test_create_sandbox_empty_name(self, client, auth_headers):
        """测试空名称"""
        response = client.post(
            "/sandbox",
            json={"name": ""},
            headers=auth_headers
        )
        
        assert response.status_code == 422  # Validation error


class TestListSandboxes:
    """测试列出 sandboxes API"""
    
    def test_list_sandboxes_success(self, client, auth_headers):
        """测试成功列出 sandboxes"""
        # Create a sandbox first
        sandbox_name = f"test_sandbox_{str(uuid7())[:8]}"
        client.post(
            "/sandbox",
            json={"name": sandbox_name},
            headers=auth_headers
        )
        
        # List sandboxes
        response = client.get("/sandbox", headers=auth_headers)
        
        assert response.status_code == 200
        data = response.json()
        assert "sandboxes" in data
        assert "total" in data
        assert data["total"] >= 1
    
    def test_list_sandboxes_with_pattern(self, client, auth_headers):
        """测试使用过滤模式"""
        response = client.get(
            "/sandbox?pattern=test_%",
            headers=auth_headers
        )
        
        assert response.status_code == 200
    
    def test_list_sandboxes_without_auth(self, client):
        """测试未认证"""
        response = client.get("/sandbox")
        
        assert response.status_code == 403


class TestGetSandbox:
    """测试获取 sandbox 信息 API"""
    
    def test_get_sandbox_success(self, client, auth_headers):
        """测试成功获取 sandbox 信息"""
        # Create a sandbox first
        sandbox_name = f"test_sandbox_{str(uuid7())[:8]}"
        create_response = client.post(
            "/sandbox",
            json={"name": sandbox_name},
            headers=auth_headers
        )
        assert create_response.status_code == 201
        
        # Get sandbox info
        response = client.get(f"/sandbox/{sandbox_name}", headers=auth_headers)
        
        assert response.status_code == 200
        data = response.json()
        assert data["sandbox_name"] == sandbox_name
    
    def test_get_sandbox_not_found(self, client, auth_headers):
        """测试获取不存在的 sandbox"""
        response = client.get("/sandbox/nonexistent", headers=auth_headers)
        
        assert response.status_code == 404
    
    def test_get_sandbox_without_auth(self, client):
        """测试未认证"""
        response = client.get("/sandbox/test")
        
        assert response.status_code == 403


class TestDeleteSandbox:
    """测试删除 sandbox API"""
    
    def test_delete_sandbox_success(self, client, auth_headers):
        """测试成功删除 sandbox"""
        # Create a sandbox first
        sandbox_name = f"test_sandbox_{str(uuid7())[:8]}"
        create_response = client.post(
            "/sandbox",
            json={"name": sandbox_name},
            headers=auth_headers
        )
        assert create_response.status_code == 201
        
        # Delete sandbox
        response = client.delete(f"/sandbox/{sandbox_name}", headers=auth_headers)
        
        assert response.status_code == 204
        
        # Verify deleted
        get_response = client.get(f"/sandbox/{sandbox_name}", headers=auth_headers)
        assert get_response.status_code == 404
    
    def test_delete_sandbox_not_found(self, client, auth_headers):
        """测试删除不存在的 sandbox"""
        response = client.delete("/sandbox/nonexistent", headers=auth_headers)
        
        assert response.status_code == 404
    
    def test_delete_sandbox_without_auth(self, client):
        """测试未认证"""
        response = client.delete("/sandbox/test")
        
        assert response.status_code == 403


class TestSandboxPermissions:
    """测试 sandbox 权限控制"""
    
    def test_user_cannot_see_others_sandboxes(self, client, db_session):
        """测试用户不能看到其他人的 sandboxes"""
        # Create two users
        user_repo = UserRepository(db_session)
        user1 = user_repo.create({
            "user_id": str(uuid7()),
            "username": f"user1_{str(uuid7())[:8]}",
            "email": f"user1_{str(uuid7())[:8]}@example.com",
            "password_hash": "hashed",
            "is_active": True
        })
        user2 = user_repo.create({
            "user_id": str(uuid7()),
            "username": f"user2_{str(uuid7())[:8]}",
            "email": f"user2_{str(uuid7())[:8]}@example.com",
            "password_hash": "hashed",
            "is_active": True
        })
        
        token1 = create_access_token({"sub": user1.user_id})
        token2 = create_access_token({"sub": user2.user_id})
        
        # User1 creates a sandbox
        sandbox_name = f"test_sandbox_{str(uuid7())[:8]}"
        response1 = client.post(
            "/sandbox",
            json={"name": sandbox_name},
            headers={"Authorization": f"Bearer {token1}"}
        )
        assert response1.status_code == 201
        
        # User2 tries to get user1's sandbox
        response2 = client.get(
            f"/sandbox/{sandbox_name}",
            headers={"Authorization": f"Bearer {token2}"}
        )
        assert response2.status_code == 403  # Forbidden
        
        # Cleanup
        try:
            db_session.delete(user1)
            db_session.delete(user2)
            db_session.commit()
        except:
            pass
