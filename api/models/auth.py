"""Auth & identity models."""

from sqlalchemy import Column, DateTime, ForeignKey, Integer, SmallInteger, String
from sqlalchemy.sql import func

from api.base import Base


class User(Base):
    __tablename__ = "users"
    user_id = Column(String(36), primary_key=True)
    username = Column(String(50), unique=True, nullable=False, index=True)
    email = Column(String(255), unique=True, nullable=False, index=True)
    password_hash = Column(String(255), nullable=False)
    display_name = Column(String(100))
    is_active = Column(SmallInteger, server_default="1", nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    last_login_at = Column(DateTime)


class Role(Base):
    __tablename__ = "roles"
    role_id = Column(String(36), primary_key=True)
    role_name = Column(String(50), unique=True, nullable=False)
    description = Column(String(255))
    created_at = Column(DateTime, default=func.now())


class UserRole(Base):
    __tablename__ = "user_roles"
    id = Column(Integer, primary_key=True, autoincrement=True)
    user_id = Column(String(36), ForeignKey("users.user_id"), nullable=False, index=True)
    role_id = Column(String(36), ForeignKey("roles.role_id"), nullable=False, index=True)
    created_at = Column(DateTime, default=func.now())


class RefreshToken(Base):
    __tablename__ = "refresh_tokens"
    token_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    token_hash = Column(String(255), nullable=False)
    expires_at = Column(DateTime, nullable=False)
    is_revoked = Column(SmallInteger, default=0, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
