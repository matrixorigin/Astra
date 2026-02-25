"""Tests for permission checker."""

import pytest
from unittest.mock import MagicMock
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


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
