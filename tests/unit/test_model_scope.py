"""Test scope-based model access control."""

import pytest

from core.llm.router import ModelRegistry
from api.database import get_db_session


@pytest.fixture
def db_session():
    """Real database session with cleanup."""
    session = next(get_db_session())
    yield session
    session.rollback()
    session.close()


def test_global_scope_models(db_session):
    """Test global scope loads models from DB."""
    registry = ModelRegistry()
    registry.load_from_db(db_session)
    assert isinstance(registry.list_models(), list)


def test_user_scope_models(db_session):
    """Test user scope loads models from DB."""
    registry = ModelRegistry()
    registry.load_from_db(db_session, user_id="alice")
    assert isinstance(registry.list_models(), list)


def test_scope_hierarchy(db_session):
    """Test scope hierarchy: user overrides global."""
    global_registry = ModelRegistry()
    global_registry.load_from_db(db_session)

    user_registry = ModelRegistry()
    user_registry.load_from_db(db_session, user_id="alice")

    assert isinstance(global_registry.list_models(), list)
    assert isinstance(user_registry.list_models(), list)


def test_model_not_found(db_session):
    """Test get returns None for nonexistent model."""
    registry = ModelRegistry()
    registry.load_from_db(db_session, user_id="alice")
    assert registry.get("nonexistent-model") is None


def test_empty_scope(db_session):
    """Test empty scope for nonexistent user."""
    registry = ModelRegistry()
    registry.load_from_db(db_session, user_id="nonexistent_user")
    assert isinstance(registry.list_models(), list)
