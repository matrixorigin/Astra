"""User management module - ORM Version."""

from datetime import datetime, timezone

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.models import RefreshToken, User
from core.auth.password import hash_password, verify_password
from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


class UserManager(DbConsumer):
    """Manage user operations using ORM."""

    def __init__(self, db_factory: DbFactory):
        """Initialize user manager.

        Args:
            db: Session instance
        """
        super().__init__(db_factory)

    def create_user(
        self,
        username: str,
        email: str,
        password: str,
        display_name: str | None = None,
    ) -> dict:
        """Create a new user using ORM.

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
        # Check if username exists using ORM
        with self._db() as db:
            existing = db.query(User).filter(User.username == username).first()
            if existing:
                raise ValueError(f"Username '{username}' already exists")

            # Check if email exists using ORM
            existing = db.query(User).filter(User.email == email).first()
            if existing:
                raise ValueError(f"Email '{email}' already exists")

            # Create user using ORM
            user_id = str(uuid7())
            password_hash = hash_password(password)

            user = User(
                user_id=user_id,
                username=username,
                email=email,
                password_hash=password_hash,
                display_name=display_name,
                is_active=True,
                created_at=datetime.now(timezone.utc)
            )

            db.add(user)
            db.commit()

            logger.info(f"Created user: {username} ({user_id})")

            return {
                "user_id": user_id,
                "username": username,
                "email": email,
                "display_name": display_name,
            }

    def authenticate_user(self, username: str, password: str) -> dict | None:
        """Authenticate user with username and password using ORM.

        Args:
            username: Username
            password: Plain text password

        Returns:
            User dictionary if authentication successful, None otherwise
        """
        with self._db() as db:
            user = db.query(User).filter(User.username == username).first()

            if not user:
                return None

            if not user.is_active:
                return None

            if not verify_password(password, user.password_hash):
                return None

            return {
                "user_id": user.user_id,
                "username": user.username,
                "email": user.email,
                "display_name": user.display_name,
                "is_active": user.is_active,
            }

    def get_user(self, user_id: str) -> dict | None:
        """Get user by ID using ORM.

        Args:
            user_id: User ID

        Returns:
            User dictionary if found, None otherwise
        """
        with self._db() as db:
            user = db.query(User).filter(User.user_id == user_id).first()

            if not user:
                return None

            return {
                "user_id": user.user_id,
                "username": user.username,
                "email": user.email,
                "display_name": user.display_name,
                "is_active": user.is_active,
                "created_at": user.created_at,
                "last_login_at": user.last_login_at,
            }

    def update_last_login(self, user_id: str) -> bool:
        """Update user's last login timestamp using ORM.

        Args:
            user_id: User ID

        Returns:
            True if updated successfully, False if user not found
        """
        with self._db() as db:
            user = db.query(User).filter(User.user_id == user_id).first()

            if not user:
                return False

            user.last_login_at = datetime.now(timezone.utc)
            db.commit()

            return True

    def deactivate_user(self, user_id: str) -> bool:
        """Deactivate user using ORM.

        Args:
            user_id: User ID

        Returns:
            True if deactivated successfully, False if user not found
        """
        with self._db() as db:
            user = db.query(User).filter(User.user_id == user_id).first()

            if not user:
                return False

            user.is_active = False
            db.commit()

            logger.info(f"Deactivated user: {user.username} ({user_id})")
            return True

    def store_refresh_token(self, user_id: str, token: str, expires_at: datetime) -> str:
        """Store refresh token using ORM.

        Args:
            user_id: User ID
            token: Refresh token
            expires_at: Token expiration time

        Returns:
            Token ID
        """
        with self._db() as db:
            from core.auth.password import hash_password

            token_id = str(uuid7())
            refresh_token = RefreshToken(
                token_id=token_id,
                user_id=user_id,
                token_hash=hash_password(token),
                expires_at=expires_at,
                is_revoked=False,
                created_at=datetime.now(timezone.utc)
            )
            db.add(refresh_token)
            db.commit()
            return token_id

    def verify_refresh_token(self, token: str) -> str | None:
        """Verify refresh token using ORM.

        Args:
            token: Refresh token

        Returns:
            User ID if token is valid, None otherwise
        """
        with self._db() as db:
            from core.auth.password import verify_password

            rt = db.query(RefreshToken).filter(RefreshToken.expires_at > datetime.now(timezone.utc)).all()

            for refresh_token in rt:
                if verify_password(token, refresh_token.token_hash) and not refresh_token.is_revoked:
                    return refresh_token.user_id

            return None

    def revoke_refresh_token(self, token: str) -> bool:
        """Revoke refresh token using ORM.

        Args:
            token: Refresh token

        Returns:
            True if revoked successfully, False if token not found
        """
        with self._db() as db:
            from core.auth.password import verify_password

            rt = db.query(RefreshToken).all()

            for refresh_token in rt:
                if verify_password(token, refresh_token.token_hash):
                    refresh_token.is_revoked = True
                    db.commit()
                    return True

            return False
