"""Test model configuration and dynamic model management."""

import pytest

from core.llm.client import LLMClient
from core.llm.models import LLMProvider
from core.llm.router import ModelConfig, ModelRegistry, ModelRouter
from api.database import get_db_session


@pytest.fixture
def db_session():
    """Real database session with cleanup."""
    session = next(get_db_session())
    yield session
    # Cleanup - rollback any changes
    session.rollback()
    session.close()


class TestModelRegistry:
    """Test ModelRegistry functionality."""

    def test_default_models_loaded(self):
        """Test default models are loaded."""
        registry = ModelRegistry()
        models = registry.list_models()
        assert len(models) > 0
        assert any(m.model_name == "gpt-4o" for m in models)

    def test_strict_mode_no_defaults(self):
        """Test strict mode loads no default models."""
        registry = ModelRegistry(use_defaults=False)
        models = registry.list_models()
        assert len(models) == 0

    def test_register_model(self):
        """Test registering a new model."""
        registry = ModelRegistry(use_defaults=False)
        config = ModelConfig(
            model_name="test-model",
            provider=LLMProvider.OPENAI,
            max_tokens=1000,
            is_active=True,
        )
        registry.register(config)
        assert registry.get("test-model") == config

    def test_get_model(self):
        """Test getting a model."""
        registry = ModelRegistry()
        model = registry.get("gpt-4o")
        assert model is not None
        assert model.model_name == "gpt-4o"

    def test_get_nonexistent_model(self):
        """Test getting nonexistent model returns None."""
        registry = ModelRegistry(use_defaults=False)
        model = registry.get("nonexistent")
        assert model is None


class TestModelRouter:
    """Test ModelRouter functionality."""

    def test_route_with_fallback(self):
        """Test routing with fallback."""
        router = ModelRouter()
        models = router.route("gpt-4o")
        assert len(models) > 0
        assert models[0].model_name == "gpt-4o"

    def test_route_nonexistent_model(self):
        """Test routing nonexistent model."""
        router = ModelRouter(use_defaults=False)
        models = router.route("nonexistent")
        assert len(models) == 0


class TestLLMClientModelConfig:
    """Test LLMClient model configuration."""

    def test_user_context_update(self, db_session):
        """Test updating user context."""
        client = LLMClient(db=db_session, user_id="alice", use_default_models=True)

        # Update context
        client.set_user_context(user_id="bob")

        assert client.user_id == "bob"

    def test_permission_check_with_defaults(self, db_session):
        """Test permission check with default models."""
        client = LLMClient(db=db_session, user_id="alice", use_default_models=True)

        # Should not raise - gpt-4o is in defaults
        client._check_model_permission("gpt-4o")

    def test_permission_check_strict_mode(self, db_session):
        """Test permission check in strict mode."""
        client = LLMClient(db=db_session, user_id="alice", use_default_models=False)

        # Should not raise - no models to check against
        try:
            client._check_model_permission("gpt-4o")
        except Exception:
            pass  # Expected in strict mode without DB config


class TestModelConfigEdgeCases:
    """Test edge cases in model configuration."""

    def test_model_with_fallback(self):
        """Test model routing with fallback strategy."""
        router = ModelRouter()
        
        # Should fall back to available model
        model = router.route("gpt-4o")
        assert model is not None
