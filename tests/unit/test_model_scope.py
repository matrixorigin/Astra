"""Test scope-based model access control."""

import json
import pytest
from core.llm.router import ModelRegistry, ModelConfig


class MockDB:
    """Mock database with scope-based configs."""

    def __init__(self):
        # Global config: only gpt-4o-mini
        self.global_config = [
            {"model_name": "gpt-4o-mini", "provider": "openai", "max_tokens": 4096}
        ]

        # Tenant config: adds claude-3-5-sonnet
        self.tenant_config = [
            {"model_name": "gpt-4o-mini", "provider": "openai", "max_tokens": 4096},
            {
                "model_name": "claude-3-5-sonnet-20241022",
                "provider": "anthropic",
                "max_tokens": 8192,
            },
        ]

        # User config: adds gpt-4o
        self.user_config = [
            {"model_name": "gpt-4o-mini", "provider": "openai", "max_tokens": 4096},
            {
                "model_name": "claude-3-5-sonnet-20241022",
                "provider": "anthropic",
                "max_tokens": 8192,
            },
            {"model_name": "gpt-4o", "provider": "openai", "max_tokens": 16384},
        ]

    def fetchone(self, query, params=None):
        if params is None:
            # Global scope
            if "scope_type = 'global'" in query:
                return {"value": json.dumps(self.global_config)}
        elif len(params) == 1:
            if "scope_type = 'user'" in query and params[0] == "alice":
                return {"value": json.dumps(self.user_config)}
            elif "scope_type = 'tenant'" in query and params[0] == "team_a":
                return {"value": json.dumps(self.tenant_config)}
        return None


@pytest.fixture
def mock_db():
    return MockDB()


def test_global_scope(mock_db):
    """Test global scope - only loads global config models."""
    registry = ModelRegistry()
    # Clear defaults to test DB loading
    registry._models.clear()
    registry.load_from_db(mock_db)

    # Should only have models from global config
    assert "gpt-4o-mini" in registry._models
    assert len(registry._models) == 1


def test_tenant_scope(mock_db):
    """Test tenant scope - loads tenant config models."""
    registry = ModelRegistry()
    registry._models.clear()
    registry.load_from_db(mock_db, tenant_id="team_a")

    # Should have models from tenant config
    assert "gpt-4o-mini" in registry._models
    assert "claude-3-5-sonnet-20241022" in registry._models
    assert len(registry._models) == 2


def test_user_scope(mock_db):
    """Test user scope - loads user config models."""
    registry = ModelRegistry()
    registry._models.clear()
    registry.load_from_db(mock_db, user_id="alice")

    # Should have all models from user config
    assert "gpt-4o-mini" in registry._models
    assert "claude-3-5-sonnet-20241022" in registry._models
    assert "gpt-4o" in registry._models
    assert len(registry._models) == 3


def test_scope_priority(mock_db):
    """Test user scope takes priority over tenant scope."""
    registry = ModelRegistry()
    registry._models.clear()
    registry.load_from_db(mock_db, user_id="alice", tenant_id="team_a")

    # Should use user config (3 models), not tenant config (2 models)
    assert len(registry._models) == 3
    assert "gpt-4o" in registry._models
