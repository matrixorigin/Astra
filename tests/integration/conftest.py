"""Shared fixtures for integration tests."""

import pytest
from uuid import uuid4

from api.repositories.user_repository import UserRepository
from core.auth.password import hash_password


@pytest.fixture
def test_user(db_session):
    """Create a test user for API tests."""
    repo = UserRepository(db_session)
    
    # Clean up any existing test user
    existing = repo.get_by_username("testuser")
    if existing:
        repo.delete(existing.user_id)
        db_session.commit()
    
    # Create new test user
    user_data = {
        "user_id": str(uuid4()),
        "username": "testuser",
        "email": "test@example.com",
        "password_hash": hash_password("testpass123"),
        "is_active": True,
    }
    
    user = repo.create(user_data)
    db_session.commit()
    
    yield user
    
    # Cleanup
    try:
        repo.delete(user.user_id)
        db_session.commit()
    except:
        pass


@pytest.fixture
def test_session(db_session, test_user):
    """Create a test session."""
    from api.repositories.session_repository import SessionRepository
    from uuid import uuid4
    
    repo = SessionRepository(db_session)
    session_data = {
        "session_id": str(uuid4()),
        "user_id": test_user.user_id,
        "agent_id": "test-agent",
        "title": "Test Session",
        "status": "active",
    }
    
    session = repo.create(**session_data)
    db_session.commit()
    
    yield session
    
    # Cleanup
    try:
        repo.delete(session.session_id)
        db_session.commit()
    except:
        pass
