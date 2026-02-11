"""Test model configuration and dynamic model management."""

import json
import pytest
from core.llm.router import ModelRouter, ModelConfig, ModelRegistry
from core.llm.models import LLMProvider
from core.llm.client import LLMClient


class MockDB:
    """Mock database for testing."""

    def __init__(self):
        self.data = {}
        self.configs = []

    def fetchone(self, query, params=None):
        # Return first matching config
        for config in self.configs:
            if "key_name" in config:
                # Check if query matches
                if "scope_type = 'global'" in query and config.get("scope_type") == "global":
                    return config
                elif "scope_type = 'user'" in query and config.get("scope_type") == "user":
                    if params and config.get("scope_id") == params[0]:
                        return config
                elif "scope_type = 'tenant'" in query and config.get("scope_type") == "tenant":
                    if params and config.get("scope_id") == params[0]:
                        return config
        return None

    def fetchall(self, query, params=None):
        return []

    def execute(self, query, params=None):
        self.data["last_query"] = query
        self.data["last_params"] = params
        return None


class TestModelRegistry:
    """Test ModelRegistry functionality."""

    def test_default_models_loaded(self):
        """Test that default models are loaded when use_defaults=True."""
        registry = ModelRegistry(use_defaults=True)
        models = registry.list_models()

        assert len(models) > 0
        model_names = [m.model_name for m in models]
        assert "gpt-4" in model_names
        assert "gpt-3.5-turbo" in model_names

    def test_strict_mode_no_defaults(self):
        """Test that no models are loaded when use_defaults=False."""
        registry = ModelRegistry(use_defaults=False)
        models = registry.list_models()

        assert len(models) == 0

    def test_register_model(self):
        """Test registering a custom model."""
        registry = ModelRegistry(use_defaults=False)

        config = ModelConfig(
            model_name="custom-model",
            provider=LLMProvider.OPENAI,
            context_window=100000,
            price_per_1k_prompt=0.01,
            price_per_1k_completion=0.03,
        )
        registry.register(config)

        models = registry.list_models()
        assert len(models) == 1
        assert models[0].model_name == "custom-model"

    def test_get_model(self):
        """Test getting a specific model."""
        registry = ModelRegistry(use_defaults=True)

        model = registry.get("gpt-4")
        assert model is not None
        assert model.model_name == "gpt-4"
        assert model.provider == LLMProvider.OPENAI

    def test_get_nonexistent_model(self):
        """Test getting a model that doesn't exist."""
        registry = ModelRegistry(use_defaults=False)

        model = registry.get("nonexistent")
        assert model is None


class TestModelRouter:
    """Test ModelRouter functionality."""

    def test_route_with_fallback(self):
        """Test routing with fallback chain."""
        db = MockDB()
        router = ModelRouter(db=db, use_defaults=True)

        chain = router.route("gpt-4", task_hint="reasoning")

        assert len(chain) > 0
        assert chain[0].model_name == "gpt-4"

    def test_route_nonexistent_model(self):
        """Test routing for a model that doesn't exist."""
        db = MockDB()
        router = ModelRouter(db=db, use_defaults=False)

        chain = router.route("nonexistent", task_hint="reasoning")

        assert len(chain) == 0


class TestLLMClientModelConfig:
    """Test LLMClient model configuration."""

    def test_user_context_update(self):
        """Test updating user context."""
        db = MockDB()
        client = LLMClient(db=db, user_id="alice", use_default_models=True)

        # Update context
        client.set_user_context(user_id="bob")

        assert client.user_id == "bob"

    def test_permission_check_with_defaults(self):
        """Test permission check when defaults are enabled."""
        db = MockDB()
        client = LLMClient(db=db, user_id="alice", use_default_models=True)

        # Should not raise - defaults are enabled
        client._check_model_permission("gpt-4")

    def test_permission_check_strict_mode(self):
        """Test permission check in strict mode."""
        db = MockDB()
        client = LLMClient(db=db, user_id="alice", use_default_models=False)

        # Should raise - no models in strict mode without DB config
        with pytest.raises(PermissionError):
            client._check_model_permission("gpt-4")


class TestModelConfigEdgeCases:
    """Test edge cases for model configuration."""

    def test_fallback_chain_execution(self):
        """Test that fallback chain is executed correctly."""
        db = MockDB()
        router = ModelRouter(db=db, use_defaults=True)

        # Get chain for a model
        chain = router.route("gpt-4", task_hint="reasoning")

        # Verify chain structure
        for model in chain:
            assert isinstance(model, ModelConfig)

    def test_model_with_fallback(self):
        """Test model configuration with fallback."""
        config = ModelConfig(
            model_name="expensive-model",
            provider=LLMProvider.OPENAI,
            fallback_to="cheap-model",
        )

        assert config.fallback_to == "cheap-model"


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
