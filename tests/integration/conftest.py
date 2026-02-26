"""Shared fixtures for integration tests."""

import os
import pytest
from uuid import uuid4

from api.repositories.user_repository import UserRepository
from core.auth.password import hash_password


def _get_worker_suffix():
    """Get unique suffix for parallel test workers."""
    worker_id = os.getenv("PYTEST_XDIST_WORKER", "master")
    return f"_{worker_id}" if worker_id != "master" else ""


@pytest.fixture
def test_user(db_session):
    """Create a test user for API tests (worker-isolated)."""
    repo = UserRepository(lambda: db_session)
    
    # Use worker-specific username/email to avoid conflicts
    worker_suffix = _get_worker_suffix()
    username = f"testuser{worker_suffix}"
    email = f"test{worker_suffix}@example.com"
    
    # Clean up any existing test user
    existing = repo.get_by_username(username)
    if existing:
        from sqlalchemy import text as _text
        db_session.execute(_text("DELETE FROM user_roles WHERE user_id = :uid"), {"uid": existing.user_id})
        repo.delete(existing.user_id)
        db_session.commit()
    
    # Create new test user
    user_data = {
        "user_id": str(uuid4()),
        "username": username,
        "email": email,
        "password_hash": hash_password("testpass123"),
        "is_active": True,
    }
    
    user = repo.create(user_data)
    db_session.commit()
    
    yield user
    
    # Cleanup (user_roles first due to FK)
    try:
        from sqlalchemy import text as _text
        db_session.execute(_text("DELETE FROM user_roles WHERE user_id = :uid"), {"uid": user.user_id})
        repo.delete(user.user_id)
        db_session.commit()
    except:
        db_session.rollback()


@pytest.fixture
def auth_headers(client, test_user):
    """Get authentication headers (worker-isolated)."""
    # Login with the worker-specific username
    response = client.post(
        "/auth/login",
        json={
            "username": test_user.username,
            "password": "testpass123",
        },
    )
    
    token = response.json()["access_token"]
    return {"Authorization": f"Bearer {token}"}


@pytest.fixture
def admin_headers(client, test_user, db_session):
    """Get admin authentication headers."""
    from sqlalchemy import text
    from core.auth.seed_roles import seed_roles

    # Ensure roles exist (may not be seeded yet in parallel runs)
    seed_roles(db_session)

    role = db_session.execute(
        text("SELECT role_id FROM roles WHERE role_name = 'mo_agent_admin' LIMIT 1")
    ).fetchone()
    if role:
        existing = db_session.execute(
            text("SELECT 1 FROM user_roles WHERE user_id = :uid AND role_id = :rid"),
            {"uid": test_user.user_id, "rid": role[0]},
        ).fetchone()
        if not existing:
            db_session.execute(
                text("INSERT INTO user_roles (user_id, role_id) VALUES (:uid, :rid)"),
                {"uid": test_user.user_id, "rid": role[0]},
            )
            db_session.commit()

    response = client.post(
        "/auth/login",
        json={"username": test_user.username, "password": "testpass123"},
    )
    token = response.json()["access_token"]
    return {"Authorization": f"Bearer {token}"}


@pytest.fixture
def test_session(db_session, test_user):
    """Create a test session."""
    from api.repositories.session_repository import SessionRepository
    from uuid import uuid4
    
    repo = SessionRepository(lambda: db_session)
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
