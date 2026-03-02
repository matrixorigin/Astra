"""Tests for user manager."""

from datetime import datetime, timedelta, timezone
from unittest.mock import MagicMock, ANY

import pytest

from core.auth.user_manager import UserManager
from api.models import User


@pytest.fixture
def mock_db_session():
    """Create a mock database session."""
    session = MagicMock()
    # Mock query chain
    # session.query(User).filter(...).first()
    session.query.return_value.filter.return_value.first.return_value = None
    return session


@pytest.fixture
def user_manager(mock_db_session):
    """Create user manager with mock database."""
    return UserManager(lambda: mock_db_session)


class TestCreateUser:
    """Test user creation."""

    def test_create_user_success(self, user_manager, mock_db_session):
        """Test successful user creation."""
        # Setup mock to return None (no existing user)
        mock_db_session.query.return_value.filter.return_value.first.return_value = None

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
        
        # Verify DB interactions
        assert mock_db_session.add.called
        assert mock_db_session.commit.called

    def test_create_user_duplicate_username(self, user_manager, mock_db_session):
        """Test creating user with duplicate username."""
        # Setup mock to return an existing user when checking username
        existing_user = User(username="existing", email="other@example.com")
        
        # Configure the mock to return existing_user for the first query (username check)
        mock_db_session.query.return_value.filter.return_value.first.side_effect = [existing_user]

        with pytest.raises(ValueError, match="Username 'existing' already exists"):
            user_manager.create_user(
                username="existing",
                email="test@example.com",
                password="password123",
            )

    def test_create_user_duplicate_email(self, user_manager, mock_db_session):
        """Test creating user with duplicate email."""
        # Setup mock: first call (username) returns None, second call (email) returns existing user
        existing_user = User(username="other", email="existing@example.com")
        mock_db_session.query.return_value.filter.return_value.first.side_effect = [None, existing_user]

        with pytest.raises(ValueError, match="Email 'existing@example.com' already exists"):
            user_manager.create_user(
                username="user2",
                email="existing@example.com",
                password="password123",
            )

    def test_create_user_without_display_name(self, user_manager, mock_db_session):
        """Test creating user without display name."""
        mock_db_session.query.return_value.filter.return_value.first.return_value = None

        user = user_manager.create_user(
            username="testuser",
            email="test@example.com",
            password="password123",
        )

        assert user["display_name"] is None


class TestAuthenticateUser:
    """Test user authentication."""

    def test_authenticate_success(self, user_manager, mock_db_session):
        """Test successful authentication."""
        # Create a real User object (detached) with hashed password
        from core.auth.password import hash_password
        password = "password123"
        user_obj = User(
            user_id="u1", 
            username="testuser", 
            password_hash=hash_password(password),
            is_active=True
        )
        
        mock_db_session.query.return_value.filter.return_value.first.return_value = user_obj

        user = user_manager.authenticate_user("testuser", password)

        assert user is not None
        assert user["username"] == "testuser"

    def test_authenticate_user_not_found(self, user_manager, mock_db_session):
        """Test authentication with non-existent user."""
        mock_db_session.query.return_value.filter.return_value.first.return_value = None

        user = user_manager.authenticate_user("nonexistent", "password123")

        assert user is None

    def test_authenticate_inactive_user(self, user_manager, mock_db_session):
        """Test authentication with inactive user."""
        user_obj = User(
            user_id="u1", 
            username="testuser", 
            is_active=False
        )
        mock_db_session.query.return_value.filter.return_value.first.return_value = user_obj

        user = user_manager.authenticate_user("testuser", "password123")

        assert user is None

    def test_authenticate_wrong_password(self, user_manager, mock_db_session):
        """Test authentication with wrong password."""
        from core.auth.password import hash_password
        user_obj = User(
            user_id="u1", 
            username="testuser", 
            password_hash=hash_password("correct"),
            is_active=True
        )
        mock_db_session.query.return_value.filter.return_value.first.return_value = user_obj

        user = user_manager.authenticate_user("testuser", "wrong")

        assert user is None


class TestGetUser:
    """Test getting user information."""

    def test_get_user_by_id_found(self, user_manager, mock_db_session):
        """Test getting user by ID when found."""
        user_obj = User(user_id="u1", username="testuser", email="test@example.com", is_active=True)
        mock_db_session.query.return_value.filter.return_value.first.return_value = user_obj

        user = user_manager.get_user("u1")

        assert user is not None
        assert user["user_id"] == "u1"
        assert user["username"] == "testuser"

    def test_get_user_by_id_not_found(self, user_manager, mock_db_session):
        """Test getting user by ID when not found."""
        mock_db_session.query.return_value.filter.return_value.first.return_value = None

        user = user_manager.get_user("nonexistent")

        assert user is None


class TestRefreshToken:
    """Test refresh token management."""

    def test_store_refresh_token(self, user_manager, mock_db_session):
        """Test storing refresh token."""
        expires_at = datetime.now(timezone.utc) + timedelta(days=7)

        token_id = user_manager.store_refresh_token("user_123", "token_abc", expires_at)

        assert token_id is not None
        assert mock_db_session.add.called
        assert mock_db_session.commit.called

    def test_verify_refresh_token_valid(self, user_manager, mock_db_session):
        """Test verifying valid refresh token."""
        from api.models import RefreshToken
        from unittest.mock import patch
        
        expires_at = datetime.now(timezone.utc) + timedelta(days=7)
        token_obj = RefreshToken(
            token_hash="hashed_token",
            user_id="user_123",
            expires_at=expires_at,
            is_revoked=False
        )
        # verify_refresh_token uses .all()
        mock_db_session.query.return_value.filter.return_value.all.return_value = [token_obj]

        with patch("core.auth.password.verify_password", return_value=True):
            user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id == "user_123"

    def test_verify_refresh_token_not_found(self, user_manager, mock_db_session):
        """Test verifying non-existent refresh token."""
        mock_db_session.query.return_value.filter.return_value.all.return_value = []

        user_id = user_manager.verify_refresh_token("invalid_token")

        assert user_id is None

    def test_verify_refresh_token_revoked(self, user_manager, mock_db_session):
        """Test verifying revoked refresh token.
        
        Revoked tokens are excluded by the SQL filter (is_revoked == 0),
        so the query returns no candidates.
        """
        mock_db_session.query.return_value.filter.return_value.all.return_value = []

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id is None

    def test_verify_refresh_token_expired(self, user_manager, mock_db_session):
        """Test verifying expired refresh token."""
        from api.models import RefreshToken
        from unittest.mock import patch
        
        # The code filters by expires_at > now, so expired tokens won't be returned by query
        mock_db_session.query.return_value.filter.return_value.all.return_value = []

        user_id = user_manager.verify_refresh_token("token_abc")

        assert user_id is None

    def test_revoke_refresh_token_success(self, user_manager, mock_db_session):
        """Test successful token revocation."""
        from api.models import RefreshToken
        from unittest.mock import patch
        
        token_obj = RefreshToken(token_hash="hashed_token", is_revoked=False)
        mock_db_session.query.return_value.filter.return_value.all.return_value = [token_obj]

        with patch("core.auth.password.verify_password", return_value=True):
            result = user_manager.revoke_refresh_token("token_abc")

        assert result is True
        assert token_obj.is_revoked is True
        assert mock_db_session.commit.called

    def test_revoke_refresh_token_not_found(self, user_manager, mock_db_session):
        """Test revoking non-existent token."""
        mock_db_session.query.return_value.filter.return_value.all.return_value = []

        result = user_manager.revoke_refresh_token("token_abc")

        assert result is False

