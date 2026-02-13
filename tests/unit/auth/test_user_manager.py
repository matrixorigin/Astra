"""Tests for user manager."""

from datetime import datetime, timedelta, timezone

import pytest

from core.auth.user_manager import UserManager
from api.database import get_db_session
from sqlalchemy import delete
from api.models import User, RefreshToken


@pytest.fixture
def db():
    """Get test database session."""
    session = next(get_db_session())
    # Clean up before test
    session.execute(delete(RefreshToken))
    session.execute(delete(User))
    session.commit()
    yield session
    # Clean up after test
    session.execute(delete(RefreshToken))
    session.execute(delete(User))
    session.commit()
    session.close()


@pytest.fixture
def user_manager(db):
    """Create user manager with real database."""
    return UserManager(db)


class TestCreateUser:
    """Test user creation."""

    def test_create_user_success(self, user_manager):
        """Test successful user creation."""
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

    def test_create_user_duplicate_username(self, user_manager):
        """Test creating user with duplicate username."""
        user_manager.create_user(
            username="existing",
            email="test@example.com",
            password="password123",
        )

        with pytest.raises(ValueError, match="Username .* already exists"):
            user_manager.create_user(
                username="existing",
                email="test2@example.com",
                password="password123",
            )

    def test_create_user_duplicate_email(self, user_manager):
        """Test creating user with duplicate email."""
        user_manager.create_user(
            username="user1",
            email="existing@example.com",
            password="password123",
        )

        with pytest.raises(ValueError, match="Email .* already exists"):
            user_manager.create_user(
                username="user2",
                email="existing@example.com",
                password="password123",
            )

    def test_create_user_without_display_name(self, user_manager):
        """Test creating user without display name."""
        user = user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="password123",
        )

        assert user["display_name"] is None


class TestAuthenticateUser:
    """Test user authentication."""

    def test_authenticate_success(self, user_manager):
        """Test successful authentication."""
        user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="password123",
            display_name="Test User",
        )

        user = user_manager.authenticate_user("testuser", "password123")

        assert user is not None
        assert user["username"] == "testuser"

    def test_authenticate_user_not_found(self, user_manager):
        """Test authentication with non-existent user."""
        user = user_manager.authenticate_user("nonexistent", "password123")

        assert user is None

    def test_authenticate_inactive_user(self, user_manager, db):
        """Test authentication with inactive user."""
        from sqlalchemy import text
        
        user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="password123",
        )
        # Deactivate user
        db.execute(text("UPDATE users SET is_active = FALSE WHERE username = 'testuser'"))
        db.commit()

        user = user_manager.authenticate_user("testuser", "password123")

        assert user is None

    def test_authenticate_wrong_password(self, user_manager):
        """Test authentication with wrong password."""
        user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="correct_password",
        )

        user = user_manager.authenticate_user("testuser", "wrong_password")

        assert user is None


class TestGetUser:
    """Test getting user information."""

    def test_get_user_by_id_found(self, user_manager):
        """Test getting user by ID when found."""
        created = user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="password123",
            display_name="Test User",
        )

        user = user_manager.get_user(created["user_id"])

        assert user is not None
        assert user["user_id"] == created["user_id"]

    def test_get_user_by_id_not_found(self, user_manager):
        """Test getting user by ID when not found."""
        user = user_manager.get_user("nonexistent")

        assert user is None


class TestRefreshToken:
    """Test refresh token management."""

    def test_store_refresh_token(self, user_manager):
        """Test storing refresh token."""
        expires_at = datetime.now(timezone.utc) + timedelta(days=7)

        token_id = user_manager.store_refresh_token("user_123", "token_abc", expires_at)

        assert token_id is not None

    def test_verify_refresh_token_valid(self, user_manager):
        """Test verifying valid refresh token."""
        expires_at = datetime.now(timezone.utc) + timedelta(days=7)
        user_manager.store_refresh_token("user_123", "token_abc", expires_at)

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id == "user_123"

    def test_verify_refresh_token_not_found(self, user_manager):
        """Test verifying non-existent refresh token."""
        user_id = user_manager.verify_refresh_token("invalid_token")

        assert user_id is None

    def test_verify_refresh_token_revoked(self, user_manager):
        """Test verifying revoked refresh token."""
        expires_at = datetime.now(timezone.utc) + timedelta(days=7)
        user_manager.store_refresh_token("user_123", "token_abc", expires_at)
        user_manager.revoke_refresh_token("token_abc")

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id is None

    def test_verify_refresh_token_expired(self, user_manager):
        """Test verifying expired refresh token."""
        expires_at = datetime.now(timezone.utc) - timedelta(days=1)
        user_manager.store_refresh_token("user_123", "token_abc", expires_at)

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id is None

    def test_revoke_refresh_token_success(self, user_manager):
        """Test revoking refresh token successfully."""
        expires_at = datetime.now(timezone.utc) + timedelta(days=7)
        user_manager.store_refresh_token("user_123", "token_abc", expires_at)

        result = user_manager.revoke_refresh_token("token_abc")

        assert result is True

    def test_revoke_refresh_token_not_found(self, user_manager):
        """Test revoking non-existent refresh token."""
        result = user_manager.revoke_refresh_token("invalid_token")

        assert result is False

