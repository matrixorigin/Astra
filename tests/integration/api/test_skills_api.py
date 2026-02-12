"""Integration tests for skills API."""

import pytest
from fastapi.testclient import TestClient

from api.main import app
from api.database import get_db_session
from api.repositories.user_repository import UserRepository


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


@pytest.fixture
def test_user(db_session):
    """Create test user."""
    repo = UserRepository(db_session)
    
    # Clean up first
    user = repo.get_by_username("skilluser")
    if user:
        repo.delete(user.user_id)
        db_session.commit()
    
    # Create user
    from core.auth.password import hash_password
    from uuid import uuid4
    
    user_data = {
        "user_id": str(uuid4()),
        "username": "skilluser",
        "email": "skill@example.com",
        "password_hash": hash_password("password123"),
        "is_active": 1,
    }
    user = repo.create(user_data)
    
    yield user
    
    # Clean up
    repo.delete(user.user_id)
    db_session.commit()


@pytest.fixture
def auth_headers(client, test_user):
    """Get authentication headers."""
    # Login to get token
    response = client.post(
        "/auth/login",
        json={
            "username": "skilluser",
            "password": "password123",
        },
    )
    
    token = response.json()["access_token"]
    return {"Authorization": f"Bearer {token}"}


class TestRegisterSkill:
    """Test skill registration endpoint."""

    def test_register_skill_without_auth(self, client):
        """Test registration without authentication."""
        response = client.post(
            "/skills",
            json={
                "skill_id": "test",
                "skill_name": "Test",
                "skill_version": "1.0.0",
                "skill_code": "pass"
            },
        )

        assert response.status_code == 403


class TestListSkills:
    """Test list skills endpoint."""

    def test_list_skills_success(self, client, auth_headers):
        """Test successful skill listing."""
        response = client.get("/skills", headers=auth_headers)

        assert response.status_code == 200
        data = response.json()
        assert "skills" in data
        assert "total" in data

    def test_list_skills_with_pagination(self, client, auth_headers):
        """Test skill listing with pagination."""
        response = client.get(
            "/skills?limit=10&offset=0",
            headers=auth_headers
        )

        assert response.status_code == 200
        data = response.json()
        assert data["limit"] == 10
        assert data["offset"] == 0


class TestGetSkill:
    """Test get skill endpoint."""

    def test_get_skill_not_found(self, client, auth_headers):
        """Test get non-existent skill."""
        response = client.get("/skills/nonexistent", headers=auth_headers)

        assert response.status_code == 404


class TestListSkillVersions:
    """Test list skill versions endpoint."""

    def test_list_versions_success(self, client, auth_headers):
        """Test successful version listing."""
        from uuid import uuid4
        
        # Register multiple versions
        skill_id = f"test_skill_{str(uuid4())[:8]}"
        
        for version in ["1.0.0", "1.1.0", "2.0.0"]:
            client.post(
                "/skills",
                headers=auth_headers,
                json={
                    "skill_id": skill_id,
                    "skill_name": "Version Test",
                    "skill_version": version,
                    "skill_code": "pass"
                },
            )

        # List versions
        response = client.get(
            f"/skills/{skill_id}/versions",
            headers=auth_headers
        )

        assert response.status_code == 200
        data = response.json()
        assert isinstance(data, list)

    def test_list_versions_empty(self, client, auth_headers):
        """Test listing versions for non-existent skill."""
        response = client.get(
            "/skills/nonexistent/versions",
            headers=auth_headers
        )

        assert response.status_code == 200
        data = response.json()
        assert data == []
