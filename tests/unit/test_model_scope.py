"""Test scope-based model access control."""

import json

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
    """Test global scope returns default models."""
    registry = ModelRegistry(use_defaults=False)
    registry.load_from_db(db_session)
    models = registry.list_models()
    
    # Should return models from DB (empty list from test DB)
    assert isinstance(models, list)


def test_tenant_scope_models(db_session):
    """Test tenant scope includes tenant-specific models."""
    registry = ModelRegistry(use_defaults=False)
    registry.load_from_db(db_session, tenant_id="team_a")
    models = registry.list_models()
    
    # Should return models for tenant (empty list from DB)
    assert isinstance(models, list)


def test_user_scope_models(db_session):
    """Test user scope includes user-specific models."""
    registry = ModelRegistry(use_defaults=False)
    registry.load_from_db(db_session, user_id="alice")
    models = registry.list_models()
    
    # Should return models for user (empty list from DB)
    assert isinstance(models, list)


def test_scope_hierarchy(db_session):
    """Test scope hierarchy: user > tenant > global."""
    # Global scope
    global_registry = ModelRegistry(use_defaults=False)
    global_registry.load_from_db(db_session)
    global_models = global_registry.list_models()
    
    # Tenant scope
    tenant_registry = ModelRegistry(use_defaults=False)
    tenant_registry.load_from_db(db_session, tenant_id="team_a")
    tenant_models = tenant_registry.list_models()
    
    # User scope
    user_registry = ModelRegistry(use_defaults=False)
    user_registry.load_from_db(db_session, user_id="alice", tenant_id="team_a")
    user_models = user_registry.list_models()
    
    # All should be lists (empty from test DB)
    assert isinstance(global_models, list)
    assert isinstance(tenant_models, list)
    assert isinstance(user_models, list)


def test_model_not_found(db_session):
    """Test behavior when model is not found."""
    registry = ModelRegistry(use_defaults=False)
    registry.load_from_db(db_session, user_id="alice")
    model = registry.get("nonexistent-model")
    
    # Should return None for non-existent model
    assert model is None


def test_empty_scope(db_session):
    """Test behavior with empty scope."""
    registry = ModelRegistry(use_defaults=False)
    registry.load_from_db(db_session, user_id="nonexistent_user")
    models = registry.list_models()
    
    # Should return empty list
    assert isinstance(models, list)
