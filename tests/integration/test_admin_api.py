"""Integration tests for admin API endpoints."""

import uuid

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text

from api.database import get_db_session, init_db
from api.main import app


@pytest.fixture(scope="module", autouse=True)
def setup_database():
    """Initialize database before tests."""
    from contextlib import suppress

    with suppress(Exception):
        init_db()  # Tables may already exist
    yield


@pytest.fixture
def client(db_session):
    """Create test client with database override."""

    def override_get_db():
        try:
            yield db_session
        finally:
            pass  # Don't close, managed by db_session fixture

    app.dependency_overrides[get_db_session] = override_get_db
    yield TestClient(app)
    app.dependency_overrides.clear()


@pytest.fixture
def admin_user(client, db_session):
    """Create admin user and return auth token."""
    unique_id = str(uuid.uuid4())[:8]
    username = f"admin_{unique_id}"
    email = f"admin_{unique_id}@test.com"

    # Register user
    response = client.post(
        "/auth/register",
        json={
            "username": username,
            "password": "admin12345",
            "email": email,
        },
    )
    assert response.status_code == 201

    # Login
    response = client.post(
        "/auth/login",
        json={"username": username, "password": "admin12345"},
    )
    assert response.status_code == 200
    token = response.json()["access_token"]

    # Decode token to get user_id
    from core.auth.jwt_manager import decode_token
    payload = decode_token(token)
    user_id = payload["sub"]

    # Grant admin role using test database session
    # Ensure mo_agent_admin role exists
    db_session.execute(
        text("""
            INSERT IGNORE INTO auth_roles (role_id, role_name, description)
            VALUES ('mo-agent-admin-role', 'mo_agent_admin', 'Administrator role')
        """)
    )
    # Assign role to user
    db_session.execute(
        text("""
            INSERT INTO auth_user_roles (user_id, role_id)
            SELECT :user_id, role_id FROM auth_roles WHERE role_name = 'mo_agent_admin'
        """),
        {"user_id": user_id},
    )
    db_session.commit()

    return {"token": token, "user_id": user_id}


@pytest.fixture
def regular_user(client):
    """Create regular user and return auth token."""
    unique_id = str(uuid.uuid4())[:8]
    username = f"regular_{unique_id}"
    email = f"regular_{unique_id}@test.com"

    # Register user
    response = client.post(
        "/auth/register",
        json={
            "username": username,
            "password": "regular12345",
            "email": email,
        },
    )
    assert response.status_code == 201

    # Login
    response = client.post(
        "/auth/login",
        json={"username": username, "password": "regular12345"},
    )
    assert response.status_code == 200
    return {"token": response.json()["access_token"]}


def test_admin_init_requires_auth(client):
    """Test that admin init requires authentication."""
    response = client.post("/admin/init")
    assert response.status_code == 401


def test_admin_init_requires_admin_role(client, regular_user):
    """Test that admin init requires admin role."""
    response = client.post(
        "/admin/init",
        headers={"Authorization": f"Bearer {regular_user['token']}"},
    )
    assert response.status_code == 403
    assert "admin role required" in response.json()["detail"].lower()


def test_admin_init_success(client, admin_user):
    """Test successful database initialization."""
    response = client.post(
        "/admin/init",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
    )
    assert response.status_code == 200
    data = response.json()
    assert "message" in data
    assert "tables_created" in data


def test_create_token_requires_admin(client, regular_user):
    """Test that token creation requires admin role."""
    response = client.post(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {regular_user['token']}"},
        json={
            "token_type": "llm",
            "provider": "openai",
            "scope": "global",
        },
    )
    assert response.status_code == 403


def test_create_and_list_tokens(client, admin_user):
    """Test creating and listing tokens."""
    # Create token
    response = client.post(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={
            "token_type": "llm",
            "provider": "openai",
            "scope": "global",
            "token_value": "sk-test123",
        },
    )
    assert response.status_code == 201
    token_data = response.json()
    assert token_data["token_type"] == "llm"
    assert token_data["provider"] == "openai"
    assert token_data["scope"] == "global"

    # List tokens
    response = client.get(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
    )
    assert response.status_code == 200
    tokens = response.json()
    assert len(tokens) > 0
    assert any(t["token_id"] == token_data["token_id"] for t in tokens)


def test_list_tokens_with_filters(client, admin_user):
    """Test listing tokens with filters."""
    # Create multiple tokens
    client.post(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"token_type": "llm", "provider": "openai", "scope": "global"},
    )
    client.post(
        "/admin/tokens",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"token_type": "api", "scope": "user", "scope_id": "user123"},
    )

    # Filter by type
    response = client.get(
        "/admin/tokens?token_type=llm",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
    )
    assert response.status_code == 200
    tokens = response.json()
    assert all(t["token_type"] == "llm" for t in tokens)

    # Filter by scope
    response = client.get(
        "/admin/tokens?scope=global",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
    )
    assert response.status_code == 200
    tokens = response.json()
    assert all(t["scope"] == "global" for t in tokens)


def test_auth_audit_logs_requires_admin(client, regular_user):
    """Test that audit logs require admin role."""
    response = client.get(
        "/admin/audit",
        headers={"Authorization": f"Bearer {regular_user['token']}"},
    )
    assert response.status_code == 403


def test_get_auth_audit_logs(client, admin_user):
    """Test getting audit logs."""
    response = client.get(
        "/admin/audit",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
    )
    assert response.status_code == 200
    logs = response.json()
    assert isinstance(logs, list)


def test_prompt_optimize(client, admin_user):
    """Test prompt optimization endpoint."""
    response = client.post(
        "/admin/prompts/optimize",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"agent_id": "test-agent", "optimization_type": "compression"},
    )
    assert response.status_code == 200
    data = response.json()
    assert "job_id" in data
    assert data["status"] == "queued"


def test_feedback_stats(client, admin_user):
    """Test feedback statistics endpoint."""
    response = client.get(
        "/admin/feedback/stats",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
    )
    assert response.status_code == 200
    data = response.json()
    assert "total_feedback" in data
    assert "positive_feedback" in data
    assert "negative_feedback" in data
    assert "feedback_by_type" in data


def test_feedback_export(client, admin_user):
    """Test feedback export endpoint."""
    response = client.post(
        "/admin/feedback/export",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"format": "jsonl"},
    )
    assert response.status_code == 200
    data = response.json()
    assert "job_id" in data
    assert data["status"] == "queued"


def test_grant_role_requires_admin(client, regular_user):
    """Test that granting roles requires admin role."""
    response = client.post(
        "/admin/users/grant-role",
        headers={"Authorization": f"Bearer {regular_user['token']}"},
        json={"username": "someuser", "role_name": "mo_agent_admin"},
    )
    assert response.status_code == 403


def test_grant_role_user_not_found(client, admin_user):
    """Test granting role to non-existent user."""
    response = client.post(
        "/admin/users/grant-role",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"username": "nonexistent", "role_name": "mo_agent_admin"},
    )
    assert response.status_code == 404
    assert "user not found" in response.json()["detail"].lower()


def test_grant_role_invalid_role(client, admin_user, db_session):
    """Test granting invalid role."""
    # Create a test user
    unique_id = str(uuid.uuid4())[:8]
    username = f"testuser_{unique_id}"
    response = client.post(
        "/auth/register",
        json={
            "username": username,
            "password": "test12345",
            "email": f"{username}@test.com",
        },
    )
    assert response.status_code == 201

    # Try to grant invalid role
    response = client.post(
        "/admin/users/grant-role",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"username": username, "role_name": "invalid_role"},
    )
    assert response.status_code == 404
    assert "role not found" in response.json()["detail"].lower()


def test_grant_and_revoke_role_success(client, admin_user, db_session):
    """Test successful role grant and revoke."""
    # Create a test user
    unique_id = str(uuid.uuid4())[:8]
    username = f"testuser_{unique_id}"
    response = client.post(
        "/auth/register",
        json={
            "username": username,
            "password": "test12345",
            "email": f"{username}@test.com",
        },
    )
    assert response.status_code == 201

    # Grant admin role
    response = client.post(
        "/admin/users/grant-role",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"username": username, "role_name": "mo_agent_admin"},
    )
    assert response.status_code == 200
    data = response.json()
    assert data["username"] == username
    assert data["role_name"] == "mo_agent_admin"
    assert "granted" in data["message"].lower()

    # Verify user has role
    result = db_session.execute(
        text("""
            SELECT 1 FROM auth_user_roles ur
            JOIN auth_users u ON ur.user_id = u.user_id
            JOIN auth_roles r ON ur.role_id = r.role_id
            WHERE u.username = :username AND r.role_name = :role_name
        """),
        {"username": username, "role_name": "mo_agent_admin"},
    ).fetchone()
    assert result is not None

    # Grant same role again (should be idempotent)
    response = client.post(
        "/admin/users/grant-role",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"username": username, "role_name": "mo_agent_admin"},
    )
    assert response.status_code == 200
    assert "already has" in response.json()["message"].lower()

    # Revoke role
    response = client.post(
        "/admin/users/revoke-role",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"username": username, "role_name": "mo_agent_admin"},
    )
    assert response.status_code == 200
    data = response.json()
    assert "revoked" in data["message"].lower()

    # Verify role was revoked
    result = db_session.execute(
        text("""
            SELECT 1 FROM auth_user_roles ur
            JOIN auth_users u ON ur.user_id = u.user_id
            JOIN auth_roles r ON ur.role_id = r.role_id
            WHERE u.username = :username AND r.role_name = :role_name
        """),
        {"username": username, "role_name": "mo_agent_admin"},
    ).fetchone()
    assert result is None

    # Revoke again (should be idempotent)
    response = client.post(
        "/admin/users/revoke-role",
        headers={"Authorization": f"Bearer {admin_user['token']}"},
        json={"username": username, "role_name": "mo_agent_admin"},
    )
    assert response.status_code == 200
    assert "does not have" in response.json()["message"].lower()


def test_first_user_becomes_admin(client, db_session):
    """Test that first registered user automatically becomes admin."""
    # Clear all users
    db_session.execute(text("DELETE FROM auth_user_roles"))
    db_session.execute(text("DELETE FROM auth_users"))
    db_session.commit()

    # Register first user
    unique_id = str(uuid.uuid4())[:8]
    username = f"firstuser_{unique_id}"
    response = client.post(
        "/auth/register",
        json={
            "username": username,
            "password": "first12345",
            "email": f"{username}@test.com",
        },
    )
    assert response.status_code == 201

    # Refresh test session to see committed data from API
    db_session.commit()

    # Verify user has admin role
    result = db_session.execute(
        text("""
            SELECT r.role_name FROM auth_user_roles ur
            JOIN auth_users u ON ur.user_id = u.user_id
            JOIN auth_roles r ON ur.role_id = r.role_id
            WHERE u.username = :username
        """),
        {"username": username},
    ).fetchone()
    assert result is not None
    assert result[0] == "mo_agent_admin"

    # Register second user
    username2 = f"seconduser_{unique_id}"
    response = client.post(
        "/auth/register",
        json={
            "username": username2,
            "password": "second12345",
            "email": f"{username2}@test.com",
        },
    )
    assert response.status_code == 201

    # Verify second user does NOT have admin role
    result = db_session.execute(
        text("""
            SELECT r.role_name FROM auth_user_roles ur
            JOIN auth_users u ON ur.user_id = u.user_id
            JOIN auth_roles r ON ur.role_id = r.role_id
            WHERE u.username = :username
        """),
        {"username": username2},
    ).fetchone()
    assert result is None
