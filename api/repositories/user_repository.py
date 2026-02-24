"""User repository with SQLAlchemy."""

from datetime import datetime, timezone

from sqlalchemy.orm import Session

from api.models import RefreshToken as RefreshTokenModel
from api.models import User as UserModel
from api.models import UserRole as UserRoleModel


class UserRepository:
    """Repository for user operations."""

    def __init__(self, db: Session):
        self.db = db

    def create(self, user_data: dict) -> UserModel:
        """Create user."""
        # Set default values for required fields
        if 'is_active' not in user_data:
            user_data['is_active'] = True

        user = UserModel(**user_data)
        self.db.add(user)
        self.db.commit()
        self.db.refresh(user)
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
        self.db.query(UserModel).filter(UserModel.user_id == user_id).update({
            "last_login_at": datetime.now(timezone.utc)
        })
        self.db.commit()

    def store_refresh_token(self, token_data: dict) -> RefreshTokenModel:
        """Store refresh token."""
        token = RefreshTokenModel(**token_data)
        self.db.add(token)
        self.db.commit()
        return token

    def get_refresh_token(self, token_hash: str) -> RefreshTokenModel | None:
        """Get refresh token by hash."""
        return self.db.query(RefreshTokenModel).filter(
            RefreshTokenModel.token_hash == token_hash,
            RefreshTokenModel.is_revoked == 0
        ).first()

    def revoke_refresh_token(self, token_hash: str) -> bool:
        """Revoke refresh token."""
        result = self.db.query(RefreshTokenModel).filter(
            RefreshTokenModel.token_hash == token_hash
        ).update({"is_revoked": 1})
        self.db.commit()
        return result > 0

    def delete(self, user_id: str) -> bool:
        """Delete user and all related tokens."""
        # Delete refresh tokens first
        self.db.query(RefreshTokenModel).filter(
            RefreshTokenModel.user_id == user_id
        ).delete()

        # Delete role assignments
        self.db.query(UserRoleModel).filter(
            UserRoleModel.user_id == user_id
        ).delete()

        # Delete user
        result = self.db.query(UserModel).filter(
            UserModel.user_id == user_id
        ).delete()

        self.db.commit()
        return result > 0
