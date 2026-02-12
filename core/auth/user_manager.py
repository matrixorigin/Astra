"""User management module."""

import hashlib
from datetime import datetime, timezone
from typing import Optional

from uuid_utils import uuid7

from core.auth.password import hash_password, verify_password
from core.logging_config import get_logger
from sdk import Database

logger = get_logger(__name__)


class UserManager:
    """Manage user operations."""

    def __init__(self, db: Database):
        """Initialize user manager.

        Args:
            db: Database instance
        """
        self.db = db

    def create_user(
        self,
        username: str,
        email: str,
        password: str,
        display_name: Optional[str] = None,
    ) -> dict:
        """Create a new user.

        Args:
            username: Unique username
            email: Unique email address
            password: Plain text password (will be hashed)
            display_name: Optional display name

        Returns:
            User dictionary with user_id, username, email

        Raises:
            ValueError: If username or email already exists
        """
        # Check if username exists
        existing = self.db.fetchone(
            "SELECT user_id FROM users WHERE username = %s", (username,)
        )
        if existing:
            raise ValueError(f"Username '{username}' already exists")

        # Check if email exists
        existing = self.db.fetchone(
            "SELECT user_id FROM users WHERE email = %s", (email,)
        )
        if existing:
            raise ValueError(f"Email '{email}' already exists")

        # Create user
        user_id = str(uuid7())
        password_hash = hash_password(password)

        self.db.execute(
            """
            INSERT INTO users (user_id, username, email, password_hash, display_name, created_at)
            VALUES (%s, %s, %s, %s, %s, %s)
            """,
            (user_id, username, email, password_hash, display_name, datetime.now(timezone.utc)),
        )

        logger.info(f"Created user: {username} ({user_id})")

        return {
            "user_id": user_id,
            "username": username,
            "email": email,
            "display_name": display_name,
        }

    def authenticate_user(self, username: str, password: str) -> Optional[dict]:
        """Authenticate user with username and password.

        Args:
            username: Username
            password: Plain text password

        Returns:
            User dictionary if authentication successful, None otherwise
        """
        user = self.db.fetchone(
            """
            SELECT user_id, username, email, password_hash, display_name, is_active
            FROM users WHERE username = %s
            """,
            (username,),
        )

        if not user:
            logger.warning(f"Authentication failed: user '{username}' not found")
            return None

        if not user["is_active"]:
            logger.warning(f"Authentication failed: user '{username}' is inactive")
            return None

        if not verify_password(password, user["password_hash"]):
            logger.warning(f"Authentication failed: invalid password for user '{username}'")
            return None

        # Update last login
        self.db.execute(
            "UPDATE users SET last_login_at = %s WHERE user_id = %s",
            (datetime.now(timezone.utc), user["user_id"]),
        )

        logger.info(f"User authenticated: {username} ({user['user_id']})")

        return {
            "user_id": user["user_id"],
            "username": user["username"],
            "email": user["email"],
            "display_name": user["display_name"],
        }

    def get_user_by_id(self, user_id: str) -> Optional[dict]:
        """Get user by ID.

        Args:
            user_id: User ID

        Returns:
            User dictionary if found, None otherwise
        """
        user = self.db.fetchone(
            """
            SELECT user_id, username, email, display_name, is_active, created_at
            FROM users WHERE user_id = %s
            """,
            (user_id,),
        )

        return user

    def get_user_by_username(self, username: str) -> Optional[dict]:
        """Get user by username.

        Args:
            username: Username

        Returns:
            User dictionary if found, None otherwise
        """
        user = self.db.fetchone(
            """
            SELECT user_id, username, email, display_name, is_active, created_at
            FROM users WHERE username = %s
            """,
            (username,),
        )

        return user

    def store_refresh_token(self, user_id: str, refresh_token: str, expires_at: datetime) -> str:
        """Store refresh token.

        Args:
            user_id: User ID
            refresh_token: Refresh token (will be hashed)
            expires_at: Token expiration time

        Returns:
            Token ID
        """
        token_id = str(uuid7())
        token_hash = hashlib.sha256(refresh_token.encode()).hexdigest()

        self.db.execute(
            """
            INSERT INTO refresh_tokens (token_id, user_id, token_hash, expires_at, created_at)
            VALUES (%s, %s, %s, %s, %s)
            """,
            (token_id, user_id, token_hash, expires_at, datetime.now(timezone.utc)),
        )

        logger.info(f"Stored refresh token for user: {user_id}")
        return token_id

    def verify_refresh_token(self, refresh_token: str) -> Optional[str]:
        """Verify refresh token and return user_id.

        Args:
            refresh_token: Refresh token

        Returns:
            User ID if token is valid, None otherwise
        """
        token_hash = hashlib.sha256(refresh_token.encode()).hexdigest()

        token = self.db.fetchone(
            """
            SELECT user_id, expires_at, is_revoked
            FROM refresh_tokens
            WHERE token_hash = %s
            """,
            (token_hash,),
        )

        if not token:
            logger.warning("Refresh token not found")
            return None

        if token["is_revoked"]:
            logger.warning("Refresh token is revoked")
            return None

        if token["expires_at"] < datetime.now(timezone.utc):
            logger.warning("Refresh token expired")
            return None

        return token["user_id"]

    def revoke_refresh_token(self, refresh_token: str) -> bool:
        """Revoke refresh token.

        Args:
            refresh_token: Refresh token

        Returns:
            True if token was revoked, False if not found
        """
        token_hash = hashlib.sha256(refresh_token.encode()).hexdigest()

        rowcount = self.db.execute(
            "UPDATE refresh_tokens SET is_revoked = TRUE WHERE token_hash = %s",
            (token_hash,),
        )

        if rowcount > 0:
            logger.info("Refresh token revoked")
            return True

        return False
