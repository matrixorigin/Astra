"""Tests for permission checker."""

import pytest
from sqlalchemy import delete

from core.auth.permission_checker import PermissionChecker
from api.database import get_db_session
from api.models import User


@pytest.fixture
def db():
    """Get test database session."""
    session = next(get_db_session())
    # Clean up before test
    session.execute(delete(User))
    session.commit()
    yield session
    # Clean up after test
    session.execute(delete(User))
    session.commit()
    session.close()


def test_can_manage_models_global(db):
    """Test global model management permission."""
    checker = PermissionChecker(db)
    
    # In development mode, all operations are allowed
    assert checker.can_manage_models("admin", "global") is True
    assert checker.can_manage_models("user", "global") is True


def test_can_manage_models_account(db):
    """Test account model management permission."""
    checker = PermissionChecker(db)
    
    # In development mode, all operations are allowed
    assert checker.can_manage_models("admin", "account", "acme") is True
    assert checker.can_manage_models("user", "account", "acme") is True


def test_can_manage_models_user(db):
    """Test user model management permission."""
    checker = PermissionChecker(db)
    
    # In development mode, all operations are allowed
    assert checker.can_manage_models("admin", "user", "alice") is True
    assert checker.can_manage_models("alice", "user", "alice") is True


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
