"""Tests for role seeding functionality."""

import pytest
from sqlalchemy import text

from core.auth.seed_roles import seed_roles, SEED_ROLES


def test_seed_roles_creates_roles(db_session):
    """Test that seed_roles creates default roles."""
    # Clear roles
    db_session.execute(text("DELETE FROM user_roles"))
    db_session.execute(text("DELETE FROM roles"))
    db_session.commit()

    # Seed roles
    count = seed_roles(db_session)
    assert count == len(SEED_ROLES)

    # Verify roles exist
    result = db_session.execute(text("SELECT role_name FROM roles ORDER BY role_name")).fetchall()
    role_names = [row[0] for row in result]
    assert "mo_agent_admin" in role_names
    assert "mo_agent_user" in role_names


def test_seed_roles_is_idempotent(db_session):
    """Test that seed_roles can be called multiple times safely."""
    # Clear roles
    db_session.execute(text("DELETE FROM user_roles"))
    db_session.execute(text("DELETE FROM roles"))
    db_session.commit()

    # First seed
    count1 = seed_roles(db_session)
    assert count1 == len(SEED_ROLES)

    # Second seed (should skip existing)
    count2 = seed_roles(db_session)
    assert count2 == 0

    # Verify still only have expected roles
    result = db_session.execute(text("SELECT COUNT(*) FROM roles")).fetchone()
    assert result[0] == len(SEED_ROLES)


def test_seed_roles_preserves_existing(db_session):
    """Test that seed_roles doesn't affect existing roles."""
    # Clear roles
    db_session.execute(text("DELETE FROM user_roles"))
    db_session.execute(text("DELETE FROM roles"))
    db_session.commit()

    # Create one role manually
    db_session.execute(
        text("INSERT INTO roles (role_id, role_name, description) VALUES (:id, :name, :desc)"),
        {"id": "role-admin", "name": "mo_agent_admin", "desc": "Admin role"}
    )
    db_session.commit()

    # Seed roles (should only create missing ones)
    count = seed_roles(db_session)
    assert count == len(SEED_ROLES) - 1  # Only creates the missing role

    # Verify all roles exist
    result = db_session.execute(text("SELECT COUNT(*) FROM roles")).fetchone()
    assert result[0] == len(SEED_ROLES)


def test_first_user_gets_admin_role(db_session):
    """Test that first registered user gets admin role."""
    from api.routers.auth import register, RegisterRequest
    from api.models import User
    
    # Clear users
    db_session.execute(text("DELETE FROM user_roles"))
    db_session.execute(text("DELETE FROM users"))
    db_session.commit()

    # Ensure roles exist
    seed_roles(db_session)

    # Register first user
    request = RegisterRequest(
        username="firstuser",
        email="first@test.com",
        password="password123",
        display_name="First User"
    )
    
    # Mock the dependency injection
    user = register(request)
    
    # Verify user has admin role
    result = db_session.execute(
        text("""
            SELECT r.role_name FROM user_roles ur
            JOIN roles r ON ur.role_id = r.role_id
            WHERE ur.user_id = :user_id
        """),
        {"user_id": user.user_id}
    ).fetchone()
    
    assert result is not None
    assert result[0] == "mo_agent_admin"


def test_second_user_no_admin_role(db_session):
    """Test that second user does not get admin role."""
    from api.routers.auth import register, RegisterRequest
    
    # Clear users
    db_session.execute(text("DELETE FROM user_roles"))
    db_session.execute(text("DELETE FROM users"))
    db_session.commit()

    # Ensure roles exist
    seed_roles(db_session)

    # Register first user
    request1 = RegisterRequest(
        username="firstuser",
        email="first@test.com",
        password="password123",
        display_name="First User"
    )
    register(request1)

    # Register second user
    request2 = RegisterRequest(
        username="seconduser",
        email="second@test.com",
        password="password123",
        display_name="Second User"
    )
    user2 = register(request2)

    # Verify second user has no roles
    result = db_session.execute(
        text("""
            SELECT r.role_name FROM user_roles ur
            JOIN roles r ON ur.role_id = r.role_id
            WHERE ur.user_id = :user_id
        """),
        {"user_id": user2.user_id}
    ).fetchone()
    
    assert result is None
