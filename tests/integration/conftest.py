"""Shared fixtures for integration tests."""

import os
import pytest
import httpx
from uuid import uuid4

from api.repositories.user_repository import UserRepository
from core.auth.jwt_manager import create_access_token
from core.auth.password import hash_password


def _get_worker_suffix():
    """Get unique suffix for parallel test workers."""
    worker_id = os.getenv("PYTEST_XDIST_WORKER", "master")
    return f"_{worker_id}" if worker_id != "master" else ""


@pytest.fixture
def http_client():
    """Create httpx client that ignores proxy settings for localhost tests."""
    with httpx.Client(trust_env=False) as client:
        yield client


@pytest.fixture
def test_user(db_session):
    """Create a test user for API tests (worker + test isolated)."""
    repo = UserRepository(lambda: db_session)

    # Use worker+fixture-specific username/email to avoid any cross-test bleed.
    worker_suffix = _get_worker_suffix()
    unique_suffix = uuid4().hex[:8]
    username = f"testuser{worker_suffix}_{unique_suffix}"
    email = f"test{worker_suffix}_{unique_suffix}@example.com"

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

        db_session.execute(
            _text("DELETE FROM auth_user_roles WHERE user_id = :uid"), {"uid": user.user_id}
        )
        repo.delete(user.user_id)
        db_session.commit()
    except:
        db_session.rollback()


@pytest.fixture
def admin_user(db_session):
    """Create a dedicated admin user for API tests."""
    from sqlalchemy import text as _text
    from api.models import Role, User, UserRole

    worker_suffix = _get_worker_suffix()
    unique_suffix = uuid4().hex[:8]
    username = f"admin{worker_suffix}_{unique_suffix}"
    email = f"admin{worker_suffix}_{unique_suffix}@example.com"

    admin_role = db_session.query(Role).filter(Role.role_name == "mo_agent_admin").first()
    if not admin_role:
        admin_role = Role(
            role_id="role-admin",
            role_name="mo_agent_admin",
            description="Administrator with full system access",
        )
        db_session.add(admin_role)
        db_session.flush()

    user = User(
        user_id=str(uuid4()),
        username=username,
        email=email,
        password_hash=hash_password("testpass123"),
        is_active=True,
    )
    db_session.add(user)
    db_session.flush()
    db_session.add(UserRole(user_id=user.user_id, role_id=admin_role.role_id))
    db_session.commit()

    yield user

    try:
        db_session.execute(
            _text("DELETE FROM auth_user_roles WHERE user_id = :uid"), {"uid": user.user_id}
        )
        db_session.execute(_text("DELETE FROM auth_refresh_tokens WHERE user_id = :uid"), {"uid": user.user_id})
        db_session.execute(_text("DELETE FROM auth_users WHERE user_id = :uid"), {"uid": user.user_id})
        db_session.commit()
    except:
        db_session.rollback()


@pytest.fixture
def auth_headers(client, test_user):
    """Get authentication headers (worker-isolated)."""
    token = create_access_token({"sub": test_user.user_id, "username": test_user.username})
    return {"Authorization": f"Bearer {token}"}


@pytest.fixture
def admin_headers(client, admin_user):
    """Get admin authentication headers."""
    token = create_access_token({"sub": admin_user.user_id, "username": admin_user.username})
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


# ── VCR + urllib3 2.x compatibility ──────────────────────────────────────────
# urllib3 2.x added `version_string` to HTTPResponse; VCR's mock response
# doesn't have it, causing AttributeError in connectionpool.py:551.
try:
    from vcr.stubs import VCRHTTPResponse

    if not hasattr(VCRHTTPResponse, "version_string"):
        VCRHTTPResponse.version_string = property(lambda self: "HTTP/1.1")
except ImportError:
    pass
