"""Integration tests for authentication API."""

from unittest.mock import MagicMock, patch

import pytest


@pytest.fixture
def mock_db():
    """Mock database for testing."""
    db = MagicMock()
    return db


@pytest.fixture
def mock_user_manager(mock_db):
    """Mock user manager for testing."""
    with patch("api.routers.auth.UserManager") as mock_class:
        manager = MagicMock()
        mock_class.return_value = manager
        yield manager


@pytest.fixture
def client():
    """Create test client."""
    # Import here to avoid circular imports
    from fastapi.testclient import TestClient
    from api.main import app
    
    return TestClient(app)


class TestRegisterEndpoint:
    """Test user registration endpoint."""

    def test_register_success(self, client, mock_user_manager):
        """Test successful user registration."""
        mock_user_manager.create_user.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "email": "test@example.com",
            "display_name": "Test User",
        }

        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
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

    def test_register_duplicate_username(self, client, mock_user_manager):
        """Test registration with duplicate username."""
        mock_user_manager.create_user.side_effect = ValueError("Username 'existing' already exists")

        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
            response = client.post(
                "/auth/register",
                json={
                    "username": "existing",
                    "email": "test@example.com",
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

    def test_login_success(self, client, mock_user_manager):
        """Test successful login."""
        mock_user_manager.authenticate_user.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "email": "test@example.com",
        }
        mock_user_manager.store_refresh_token.return_value = "token_id"

        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
            response = client.post(
                "/auth/login",
                json={
                    "username": "testuser",
                    "password": "password123",
                },
            )

        assert response.status_code == 200
        data = response.json()
        assert "access_token" in data
        assert "refresh_token" in data
        assert data["token_type"] == "bearer"
        assert "expires_in" in data

    def test_login_invalid_credentials(self, client, mock_user_manager):
        """Test login with invalid credentials."""
        mock_user_manager.authenticate_user.return_value = None

        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
            response = client.post(
                "/auth/login",
                json={
                    "username": "testuser",
                    "password": "wrongpassword",
                },
            )

        assert response.status_code == 401
        assert "Invalid username or password" in response.json()["detail"]


class TestRefreshEndpoint:
    """Test token refresh endpoint."""

    def test_refresh_success(self, client, mock_user_manager):
        """Test successful token refresh."""
        from core.auth.jwt_manager import create_refresh_token

        refresh_token = create_refresh_token({"sub": "user_123"})

        mock_user_manager.verify_refresh_token.return_value = "user_123"
        mock_user_manager.get_user_by_id.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "email": "test@example.com",
        }
        mock_user_manager.revoke_refresh_token.return_value = True
        mock_user_manager.store_refresh_token.return_value = "new_token_id"

        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
            response = client.post(
                "/auth/refresh",
                json={"refresh_token": refresh_token},
            )

        assert response.status_code == 200
        data = response.json()
        assert "access_token" in data
        assert "refresh_token" in data

    def test_refresh_invalid_token(self, client, mock_user_manager):
        """Test refresh with invalid token."""
        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
            response = client.post(
                "/auth/refresh",
                json={"refresh_token": "invalid_token"},
            )

        assert response.status_code == 401

    def test_refresh_access_token_rejected(self, client, mock_user_manager):
        """Test refresh with access token (wrong type)."""
        from core.auth.jwt_manager import create_access_token

        access_token = create_access_token({"sub": "user_123"})

        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
            response = client.post(
                "/auth/refresh",
                json={"refresh_token": access_token},
            )

        assert response.status_code == 401
        assert "Invalid token type" in response.json()["detail"]


class TestLogoutEndpoint:
    """Test logout endpoint."""

    def test_logout_success(self, client, mock_user_manager):
        """Test successful logout."""
        mock_user_manager.revoke_refresh_token.return_value = True

        with patch("api.routers.auth.get_user_manager", return_value=mock_user_manager):
            response = client.post(
                "/auth/logout",
                json={"refresh_token": "some_token"},
            )

        assert response.status_code == 200
        assert "Logged out successfully" in response.json()["message"]


class TestHealthEndpoint:
    """Test health check endpoint."""

    def test_health_check_healthy(self, client):
        """Test health check when database is healthy."""
        # Health check creates Database inside the function, so we mock at import time
        response = client.get("/health")

        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "healthy"
        assert data["database"] == "connected"

    def test_health_check_unhealthy(self, client):
        """Test health check when database is unhealthy."""
        # Mock Database in sdk module (imported inside health_check function)
        with patch("sdk.Database") as mock_db_class:
            mock_db = MagicMock()
            mock_db.get_connection.return_value.__enter__.side_effect = Exception("Connection failed")
            mock_db_class.return_value = mock_db

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
