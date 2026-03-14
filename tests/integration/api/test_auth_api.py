"""Integration tests for authentication API."""

from unittest.mock import MagicMock, patch

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


@pytest.fixture(autouse=True)
def cleanup_test_users(db_session):
    """Clean up test users before and after each test."""
    repo = UserRepository(lambda: db_session)

    # Clean before
    test_usernames = ["testuser", "existing", "loginuser", "refreshuser", "logoutuser"]
    for username in test_usernames:
        user = repo.get_by_username(username)
        if user:
            repo.delete(user.user_id)
    db_session.commit()

    yield

    # Clean after
    for username in test_usernames:
        user = repo.get_by_username(username)
        if user:
            repo.delete(user.user_id)
    db_session.commit()


class TestRegisterEndpoint:
    """Test user registration endpoint."""

    def test_register_success(self, client):
        """Test successful user registration."""
        response = client.post(
            "/auth/register",
            json={
                "username": "testuser",
                "email": "test@example.com",
                "password": "password123",
                "display_name": "Test User",
            },
        )

        assert response.status_code == 201
        data = response.json()
        assert data["username"] == "testuser"
        assert data["email"] == "test@example.com"
        assert "user_id" in data

    def test_register_duplicate_username(self, client):
        """Test registration with duplicate username."""
        # First registration
        client.post(
            "/auth/register",
            json={
                "username": "existing",
                "email": "test1@example.com",
                "password": "password123",
            },
        )

        # Duplicate registration
        response = client.post(
            "/auth/register",
            json={
                "username": "existing",
                "email": "test2@example.com",
                "password": "password123",
            },
        )

        assert response.status_code == 400
        assert "already exists" in response.json()["detail"]

    def test_register_invalid_username(self, client):
        """Test registration with invalid username."""
        response = client.post(
            "/auth/register",
            json={
                "username": "ab",  # Too short
                "email": "test@example.com",
                "password": "password123",
            },
        )

        assert response.status_code == 422

    def test_register_invalid_email(self, client):
        """Test registration with invalid email."""
        response = client.post(
            "/auth/register",
            json={
                "username": "testuser",
                "email": "invalid-email",
                "password": "password123",
            },
        )

        assert response.status_code == 422

    def test_register_short_password(self, client):
        """Test registration with short password."""
        response = client.post(
            "/auth/register",
            json={
                "username": "testuser",
                "email": "test@example.com",
                "password": "short",
            },
        )

        assert response.status_code == 422


class TestLoginEndpoint:
    """Test user login endpoint."""

    def test_login_success(self, client):
        """Test successful login."""
        # Register user first
        client.post(
            "/auth/register",
            json={
                "username": "loginuser",
                "email": "login@example.com",
                "password": "password123",
            },
        )

        # Login
        response = client.post(
            "/auth/login",
            json={
                "username": "loginuser",
                "password": "password123",
            },
        )

        assert response.status_code == 200
        data = response.json()
        assert "access_token" in data
        assert "refresh_token" in data
        assert data["token_type"] == "bearer"
        assert "expires_in" in data

    def test_login_invalid_credentials(self, client):
        """Test login with invalid credentials."""
        # Register user first
        client.post(
            "/auth/register",
            json={
                "username": "loginuser",
                "email": "login@example.com",
                "password": "password123",
            },
        )

        # Login with wrong password
        response = client.post(
            "/auth/login",
            json={
                "username": "loginuser",
                "password": "wrongpassword",
            },
        )

        assert response.status_code == 401
        assert "Invalid username or password" in response.json()["detail"]


class TestRefreshEndpoint:
    """Test token refresh endpoint."""

    def test_refresh_success(self, client):
        """Test successful token refresh."""
        # Register and login
        client.post(
            "/auth/register",
            json={
                "username": "refreshuser",
                "email": "refresh@example.com",
                "password": "password123",
            },
        )

        login_response = client.post(
            "/auth/login",
            json={
                "username": "refreshuser",
                "password": "password123",
            },
        )

        refresh_token = login_response.json()["refresh_token"]

        # Refresh token
        response = client.post(
            "/auth/refresh",
            json={"refresh_token": refresh_token},
        )

        assert response.status_code == 200
        data = response.json()
        assert "access_token" in data
        assert "refresh_token" in data

    def test_refresh_invalid_token(self, client):
        """Test refresh with invalid token."""
        response = client.post(
            "/auth/refresh",
            json={"refresh_token": "invalid_token"},
        )

        assert response.status_code == 401

    def test_refresh_access_token_rejected(self, client):
        """Test refresh with access token (wrong type)."""
        # Register and login
        client.post(
            "/auth/register",
            json={
                "username": "refreshuser",
                "email": "refresh@example.com",
                "password": "password123",
            },
        )

        login_response = client.post(
            "/auth/login",
            json={
                "username": "refreshuser",
                "password": "password123",
            },
        )

        access_token = login_response.json()["access_token"]

        # Try to refresh with access token
        response = client.post(
            "/auth/refresh",
            json={"refresh_token": access_token},
        )

        assert response.status_code == 401
        assert "Invalid token type" in response.json()["detail"]


class TestLogoutEndpoint:
    """Test logout endpoint."""

    def test_logout_success(self, client):
        """Test successful logout."""
        # Register and login
        client.post(
            "/auth/register",
            json={
                "username": "logoutuser",
                "email": "logout@example.com",
                "password": "password123",
            },
        )

        login_response = client.post(
            "/auth/login",
            json={
                "username": "logoutuser",
                "password": "password123",
            },
        )

        refresh_token = login_response.json()["refresh_token"]

        # Logout
        response = client.post(
            "/auth/logout",
            json={"refresh_token": refresh_token},
        )

        assert response.status_code == 200
        assert "Logged out successfully" in response.json()["message"]


class TestHealthEndpoint:
    """Test health check endpoint."""

    def test_health_check_healthy(self, client):
        """Test health check when database is healthy."""
        response = client.get("/health")

        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "healthy"
        assert data["database"] == "connected"

    def test_health_check_unhealthy(self, client):
        """Test health check when database is unhealthy."""
        with patch("api.database.engine.connect") as mock_connect:
            mock_connect.side_effect = Exception("Connection failed")

            response = client.get("/health")

            assert response.status_code == 200
            data = response.json()
            assert data["status"] == "unhealthy"
            assert data["database"] == "disconnected"


class TestRootEndpoint:
    """Test root endpoint."""

    def test_root(self, client):
        """Test root endpoint."""
        response = client.get("/")

        assert response.status_code == 200
        data = response.json()
        assert data["name"] == "Agent Engine API"
        assert "version" in data
        assert "docs" in data
