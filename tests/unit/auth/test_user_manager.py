"""Tests for user manager."""

from datetime import datetime, timedelta, timezone
from unittest.mock import MagicMock, patch

import pytest

from core.auth.user_manager import UserManager


@pytest.fixture
def mock_db():
    """Mock database for testing."""
    return MagicMock()


@pytest.fixture
def user_manager(mock_db):
    """Create user manager with mock database."""
    return UserManager(mock_db)


class TestCreateUser:
    """Test user creation."""

    def test_create_user_success(self, user_manager, mock_db):
        """Test successful user creation."""
        mock_db.fetchone.return_value = None  # No existing user
        mock_db.execute.return_value = 1

        user = user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="password123",
            display_name="Test User",
        )

        assert user["username"] == "testuser"
        assert user["email"] == "test@example.com"
        assert user["display_name"] == "Test User"
        assert "user_id" in user
        mock_db.execute.assert_called_once()

    def test_create_user_duplicate_username(self, user_manager, mock_db):
        """Test creating user with duplicate username."""
        mock_db.fetchone.return_value = {"user_id": "existing"}

        with pytest.raises(ValueError, match="Username .* already exists"):
            user_manager.create_user(
                username="existing",
                email="test@example.com",
                password="password123",
            )

    def test_create_user_duplicate_email(self, user_manager, mock_db):
        """Test creating user with duplicate email."""
        mock_db.fetchone.side_effect = [None, {"user_id": "existing"}]

        with pytest.raises(ValueError, match="Email .* already exists"):
            user_manager.create_user(
                username="testuser",
                email="existing@example.com",
                password="password123",
            )

    def test_create_user_without_display_name(self, user_manager, mock_db):
        """Test creating user without display name."""
        mock_db.fetchone.return_value = None
        mock_db.execute.return_value = 1

        user = user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="password123",
        )

        assert user["display_name"] is None


class TestAuthenticateUser:
    """Test user authentication."""

    def test_authenticate_success(self, user_manager, mock_db):
        """Test successful authentication."""
        from core.auth.password import hash_password

        password_hash = hash_password("password123")
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "email": "test@example.com",
            "password_hash": password_hash,
            "display_name": "Test User",
            "is_active": True,
        }
        mock_db.execute.return_value = 1

        user = user_manager.authenticate_user("testuser", "password123")

        assert user is not None
        assert user["user_id"] == "user_123"
        assert user["username"] == "testuser"
        mock_db.execute.assert_called_once()  # Update last_login_at

    def test_authenticate_user_not_found(self, user_manager, mock_db):
        """Test authentication with non-existent user."""
        mock_db.fetchone.return_value = None

        user = user_manager.authenticate_user("nonexistent", "password123")

        assert user is None

    def test_authenticate_inactive_user(self, user_manager, mock_db):
        """Test authentication with inactive user."""
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "password_hash": "hash",
            "is_active": False,
        }

        user = user_manager.authenticate_user("testuser", "password123")

        assert user is None

    def test_authenticate_wrong_password(self, user_manager, mock_db):
        """Test authentication with wrong password."""
        from core.auth.password import hash_password

        password_hash = hash_password("correct_password")
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "password_hash": password_hash,
            "is_active": True,
        }

        user = user_manager.authenticate_user("testuser", "wrong_password")

        assert user is None


class TestGetUser:
    """Test getting user information."""

    def test_get_user_by_id_found(self, user_manager, mock_db):
        """Test getting user by ID when found."""
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "email": "test@example.com",
            "display_name": "Test User",
            "is_active": True,
            "created_at": datetime.now(timezone.utc),
        }

        user = user_manager.get_user_by_id("user_123")

        assert user is not None
        assert user["user_id"] == "user_123"

    def test_get_user_by_id_not_found(self, user_manager, mock_db):
        """Test getting user by ID when not found."""
        mock_db.fetchone.return_value = None

        user = user_manager.get_user_by_id("nonexistent")

        assert user is None

    def test_get_user_by_username_found(self, user_manager, mock_db):
        """Test getting user by username when found."""
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "username": "testuser",
            "email": "test@example.com",
        }

        user = user_manager.get_user_by_username("testuser")

        assert user is not None
        assert user["username"] == "testuser"

    def test_get_user_by_username_not_found(self, user_manager, mock_db):
        """Test getting user by username when not found."""
        mock_db.fetchone.return_value = None

        user = user_manager.get_user_by_username("nonexistent")

        assert user is None


class TestRefreshToken:
    """Test refresh token management."""

    def test_store_refresh_token(self, user_manager, mock_db):
        """Test storing refresh token."""
        mock_db.execute.return_value = 1
        expires_at = datetime.now(timezone.utc) + timedelta(days=7)

        token_id = user_manager.store_refresh_token("user_123", "token_abc", expires_at)

        assert token_id is not None
        mock_db.execute.assert_called_once()

    def test_verify_refresh_token_valid(self, user_manager, mock_db):
        """Test verifying valid refresh token."""
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "expires_at": datetime.now(timezone.utc) + timedelta(days=7),
            "is_revoked": False,
        }

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id == "user_123"

    def test_verify_refresh_token_not_found(self, user_manager, mock_db):
        """Test verifying non-existent refresh token."""
        mock_db.fetchone.return_value = None

        user_id = user_manager.verify_refresh_token("invalid_token")

        assert user_id is None

    def test_verify_refresh_token_revoked(self, user_manager, mock_db):
        """Test verifying revoked refresh token."""
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "expires_at": datetime.now(timezone.utc) + timedelta(days=7),
            "is_revoked": True,
        }

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id is None

    def test_verify_refresh_token_expired(self, user_manager, mock_db):
        """Test verifying expired refresh token."""
        mock_db.fetchone.return_value = {
            "user_id": "user_123",
            "expires_at": datetime.now(timezone.utc) - timedelta(days=1),
            "is_revoked": False,
        }

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id is None

    def test_revoke_refresh_token_success(self, user_manager, mock_db):
        """Test revoking refresh token successfully."""
        mock_db.execute.return_value = 1

        result = user_manager.revoke_refresh_token("token_abc")

        assert result is True
        mock_db.execute.assert_called_once()

    def test_revoke_refresh_token_not_found(self, user_manager, mock_db):
        """Test revoking non-existent refresh token."""
        mock_db.execute.return_value = 0

        result = user_manager.revoke_refresh_token("invalid_token")

        assert result is False
