"""Tests for permission checker."""

from unittest.mock import MagicMock

import pytest

from api.dependencies import _load_user_with_fresh_session
from core.auth.permission_checker import has_role_in_session
from core.auth.permission_checker import PermissionChecker


@pytest.fixture
def permission_checker():
    """Permission checker fixture with mocked DB session."""
    mock_db = MagicMock()
    return PermissionChecker(lambda: mock_db)


def test_can_manage_models_global(permission_checker):
    """Test global model management permission."""
    checker = permission_checker

    # Global scope: Admin only
    # We mock is_admin to test logic
    checker.is_admin = lambda user_id: user_id == "admin"

    assert checker.can_manage_models("admin", "global") is True
    assert checker.can_manage_models("user", "global") is False


def test_can_manage_models_account(permission_checker):
    """Test account model management permission."""
    checker = permission_checker

    # Account scope: Admin only (for now)
    checker.is_admin = lambda user_id: user_id == "admin"

    assert checker.can_manage_models("admin", "account", "acme") is True
    assert checker.can_manage_models("user", "account", "acme") is False


def test_can_manage_models_user(permission_checker):
    """Test user model management permission."""
    checker = permission_checker

    # User scope: Self or Admin
    checker.is_admin = lambda user_id: user_id == "admin"
    checker.is_user = lambda user_id: True  # Assume valid user

    assert checker.can_manage_models("admin", "user", "alice") is True
    assert checker.can_manage_models("alice", "user", "alice") is True
    assert checker.can_manage_models("bob", "user", "alice") is False


def test_has_role_in_session_retries_with_fresh_session(monkeypatch):
    stale_db = MagicMock()
    stale_query = stale_db.query.return_value
    stale_query.join.return_value = stale_query
    stale_query.filter.return_value = stale_query
    stale_query.count.return_value = 0

    fresh_db = MagicMock()
    fresh_query = fresh_db.query.return_value
    fresh_query.join.return_value = fresh_query
    fresh_query.filter.return_value = fresh_query
    fresh_query.count.return_value = 1

    monkeypatch.setattr("core.auth.permission_checker.SessionLocal", lambda: fresh_db)

    assert has_role_in_session(stale_db, "user-1", "mo_agent_admin") is True
    fresh_db.close.assert_called_once()


def test_load_user_with_fresh_session_falls_back_to_username(monkeypatch):
    fresh_db = MagicMock()
    repo = MagicMock()
    repo.get_by_id.return_value = None
    repo.get_by_username.return_value = object()

    monkeypatch.setattr("api.dependencies.SessionLocal", lambda: fresh_db)
    monkeypatch.setattr("api.dependencies.UserRepository", lambda factory: repo)

    assert _load_user_with_fresh_session("user-1", "alice") is repo.get_by_username.return_value
    repo.get_by_id.assert_called_once_with("user-1")
    repo.get_by_username.assert_called_once_with("alice")
    fresh_db.close.assert_called_once()


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
