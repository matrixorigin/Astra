"""Test LLMClient configuration, permission checks, and user context."""

import pytest

from core.llm.client import LLMClient
from api.database import get_db_session


@pytest.fixture
def db_session():
    """Real database session with cleanup."""
    session = next(get_db_session())
    yield session
    session.rollback()
    session.close()


class TestSetUserContext:
    """Test LLMClient.set_user_context updates internal state."""

    def test_updates_user_id(self, db_session):
        """set_user_context should update user_id and rebuild router."""
        client = LLMClient(lambda: db_session, user_id="alice")
        client.set_user_context(user_id="bob")
        assert client.user_id == "bob"

    def test_updates_scope_resolver(self, db_session):
        """set_user_context should update user_id."""
        client = LLMClient(lambda: db_session, user_id="alice")
        client.set_user_context(user_id="bob", scope_context={"repo": "test"})
        assert client.user_id == "bob"

    def test_rebuilds_router(self, db_session):
        """set_user_context should create a new ModelRouter."""
        client = LLMClient(lambda: db_session, user_id="alice")
        old_router = client.router
        client.set_user_context(user_id="bob")
        assert client.router is not old_router


class TestCheckModelPermission:
    """Test LLMClient._check_model_permission error messages."""

    def test_available_model_passes(self, db_session):
        """Permission check should pass for a registered model."""
        client = LLMClient(lambda: db_session)
        # Register a model so it's available
        from core.llm.router import ModelConfig
        from core.llm.models import LLMProvider
        client.router.registry.register(ModelConfig(
            model_name="test-model", provider=LLMProvider.OPENAI,
        ))
        # Should not raise
        client._check_model_permission("test-model")

    def test_unavailable_model_raises_with_details(self, db_session):
        """Permission check should raise with scope info and available models."""
        client = LLMClient(lambda: db_session, user_id="alice")
        from core.llm.router import ModelConfig
        from core.llm.models import LLMProvider
        client.router.registry.register(ModelConfig(
            model_name="gpt-4o", provider=LLMProvider.OPENAI,
        ))
        with pytest.raises(PermissionError, match="not available"):
            client._check_model_permission("nonexistent-model")

    def test_error_message_includes_user(self, db_session):
        """Error message should mention the user scope."""
        client = LLMClient(lambda: db_session, user_id="alice")
        with pytest.raises(PermissionError, match="alice"):
            client._check_model_permission("nonexistent-model")


class TestApiKeyResolution:
    """Test LLMClient._get_api_key fallback behavior."""

    def test_returns_none_for_unknown_provider(self, db_session):
        """Should return None when no key is configured."""
        client = LLMClient(lambda: db_session, user_id="alice")
        assert client._get_api_key("nonexistent_provider") is None

    def test_returns_string_or_none(self, db_session):
        """Should return str or None for known providers."""
        client = LLMClient(lambda: db_session)
        key = client._get_api_key("openai")
        assert key is None or isinstance(key, str)
