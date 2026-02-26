"""User repository with SQLAlchemy."""

from collections.abc import Callable
from datetime import datetime, timezone

from sqlalchemy.orm import Session

from api.models import RefreshToken as RefreshTokenModel
from api.models import User as UserModel
from api.models import UserRole as UserRoleModel


class UserRepository:
    """Repository for user operations.

    Accepts a ``db_factory`` that returns the *current* request-scoped session.
    See ``AgentRepository`` for the db_factory contract.
    """

    def __init__(self, db_factory: Callable[[], Session]):
        self._db_factory = db_factory

    @property
    def db(self) -> Session:
        return self._db_factory()

    def create(self, user_data: dict) -> UserModel:
        """Create user."""
        if 'is_active' not in user_data:
            user_data['is_active'] = True
        db = self.db
        user = UserModel(**user_data)
        db.add(user)
        db.commit()
        db.refresh(user)
        return user

    def get_by_id(self, user_id: str) -> UserModel | None:
        """Get user by ID."""
        return self.db.query(UserModel).filter(UserModel.user_id == user_id).first()

    def get_by_username(self, username: str) -> UserModel | None:
        """Get user by username."""
        return self.db.query(UserModel).filter(UserModel.username == username).first()

    def get_by_email(self, email: str) -> UserModel | None:
        """Get user by email."""
        return self.db.query(UserModel).filter(UserModel.email == email).first()

    def update_last_login(self, user_id: str) -> None:
        """Update last login time."""
        db = self.db
        db.query(UserModel).filter(UserModel.user_id == user_id).update({
            "last_login_at": datetime.now(timezone.utc)
        })
        db.commit()

    def store_refresh_token(self, token_data: dict) -> RefreshTokenModel:
        """Store refresh token."""
        db = self.db
        token = RefreshTokenModel(**token_data)
        db.add(token)
        db.commit()
        return token

    def get_refresh_token(self, token_hash: str) -> RefreshTokenModel | None:
        """Get refresh token by hash."""
        return self.db.query(RefreshTokenModel).filter(
            RefreshTokenModel.token_hash == token_hash,
            RefreshTokenModel.is_revoked == 0
        ).first()

    def revoke_refresh_token(self, token_hash: str) -> bool:
        """Revoke refresh token."""
        db = self.db
        result = db.query(RefreshTokenModel).filter(
            RefreshTokenModel.token_hash == token_hash
        ).update({"is_revoked": 1})
        db.commit()
        return result > 0

    def delete(self, user_id: str) -> bool:
        """Delete user and all related tokens."""
        db = self.db
        db.query(RefreshTokenModel).filter(RefreshTokenModel.user_id == user_id).delete()
        db.query(UserRoleModel).filter(UserRoleModel.user_id == user_id).delete()
        result = db.query(UserModel).filter(UserModel.user_id == user_id).delete()
        db.commit()
        return result > 0
