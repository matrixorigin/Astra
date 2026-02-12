"""SQLAlchemy ORM models for authentication and agent management."""

from datetime import datetime

from sqlalchemy import Boolean, Column, DateTime, Integer, String, Text, JSON
from sqlalchemy.ext.declarative import declarative_base
from sqlalchemy.sql import func

Base = declarative_base()


class User(Base):
    """User model."""
    
    __tablename__ = "users"
    
    user_id = Column(String(36), primary_key=True)
    username = Column(String(50), unique=True, nullable=False, index=True)
    email = Column(String(255), unique=True, nullable=False, index=True)
    password_hash = Column(String(255), nullable=False)
    display_name = Column(String(100))
    is_active = Column(Boolean, default=True, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    last_login_at = Column(DateTime)


class Agent(Base):
    """Agent model."""
    
    __tablename__ = "agents"
    
    agent_id = Column(String(36), primary_key=True)
    agent_name = Column(String(100), nullable=False)
    agent_type = Column(String(50), nullable=False)
    owner_user_id = Column(String(36), nullable=False, index=True)
    config = Column(JSON)
    is_active = Column(Boolean, default=True, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class RefreshToken(Base):
    """Refresh token model."""
    
    __tablename__ = "refresh_tokens"
    
    token_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    token_hash = Column(String(255), nullable=False)
    expires_at = Column(DateTime, nullable=False)
    is_revoked = Column(Boolean, default=False, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)


class Session(Base):
    """Session model."""
    
    __tablename__ = "sessions"
    
    session_id = Column(String(36), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)
    status = Column(String(20), default="active", nullable=False, index=True)
    event_count = Column(Integer, default=0, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    last_active_at = Column(DateTime, default=func.now(), nullable=False)
    closed_at = Column(DateTime)
    metadata = Column(JSON)


class Event(Base):
    """Event model."""
    
    __tablename__ = "conversation_events"
    
    event_id = Column(String(36), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    user_id = Column(String(36), nullable=False, index=True)
    event_type = Column(String(50), nullable=False, index=True)
    content = Column(Text, nullable=False)
    created_at = Column(DateTime, default=func.now(), nullable=False)
    metadata = Column(JSON)
    parent_event_id = Column(String(36), index=True)
    causal_chain_id = Column(String(36), index=True)
