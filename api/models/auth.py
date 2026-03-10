"""Auth & identity models."""

from sqlalchemy import Column, ForeignKey, Integer, SmallInteger, String
from sqlalchemy.sql import func

from api.base import Base
from api.models._types import DateTime6, NullableJSON as JSON


class User(Base):
    __tablename__ = "auth_users"
    user_id = Column(String(36), primary_key=True)
    username = Column(String(50), unique=True, nullable=False, index=True)
    email = Column(String(255), unique=True, nullable=False, index=True)
    password_hash = Column(String(255), nullable=False)
    display_name = Column(String(100))
    is_active = Column(SmallInteger, server_default="1", nullable=False)
    created_at = Column(DateTime6, default=func.now(), nullable=False)
    last_login_at = Column(DateTime6)


class Role(Base):
    __tablename__ = "auth_roles"
    role_id = Column(String(36), primary_key=True)
    role_name = Column(String(50), unique=True, nullable=False)
    description = Column(String(255))
    created_at = Column(DateTime6, default=func.now())


class UserRole(Base):
    __tablename__ = "auth_user_roles"
    id = Column(Integer, primary_key=True, autoincrement=True)
    user_id = Column(String(36), ForeignKey("auth_users.user_id"), nullable=False, index=True)
    role_id = Column(String(36), ForeignKey("auth_roles.role_id"), nullable=False, index=True)
    created_at = Column(DateTime6, default=func.now())


class RefreshToken(Base):
    __tablename__ = "auth_refresh_tokens"
    token_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    token_hash = Column(String(255), nullable=False)
    token_prefix = Column(String(16), nullable=True, index=True)
    expires_at = Column(DateTime6, nullable=False, index=True)
    is_revoked = Column(SmallInteger, default=0, server_default="0", nullable=False)
    created_at = Column(DateTime6, default=func.now(), nullable=False)


class AuditLog(Base):
    __tablename__ = "auth_audit_logs"
    log_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    action = Column(String(50), nullable=False)
    resource_type = Column(String(50))
    resource_id = Column(String(64))
    details = Column(JSON)
    ip_address = Column(String(45))
    created_at = Column(DateTime6, default=func.now(), index=True)


class Token(Base):
    __tablename__ = "auth_tokens"
    token_id = Column(String(36), primary_key=True)
    type = Column(String(50), nullable=False)
    provider = Column(String(50), nullable=False)
    encrypted_value = Column(String(255), nullable=True)
    secret_ref = Column(String(255))
    is_active = Column(SmallInteger, default=1, server_default="1")
    scope_user_id = Column(String(36), index=True)
    scope_repo = Column(String(255), index=True)
    created_at = Column(DateTime6, default=func.now())
    expires_at = Column(DateTime6, nullable=True)
    token_metadata = Column("metadata", JSON)


class ApiKey(Base):
    """API key for programmatic access (SaaS mode)."""

    __tablename__ = "auth_api_keys"
    key_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    key_hash = Column(String(255), nullable=False, unique=True)
    key_prefix = Column(String(12), nullable=False, index=True)
    name = Column(String(100), nullable=False)
    is_active = Column(SmallInteger, default=1, server_default="1", nullable=False)
    created_at = Column(DateTime6, default=func.now(), nullable=False)
    expires_at = Column(DateTime6, nullable=True)
    last_used_at = Column(DateTime6, nullable=True)
