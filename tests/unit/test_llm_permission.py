"""Test LLM client permission control."""

import pytest

from core.llm.client import LLMClient
from tests.unit.test_model_scope import MockDB


def test_permission_check_allowed():
    """Test user can use models in their scope."""
    db = MockDB()
    # Use strict mode (no defaults)
    client = LLMClient(db=db, user_id="alice", use_default_models=False)

    # Should not raise - gpt-4o is in alice's scope
    client._check_model_permission("gpt-4o")


def test_permission_check_denied():
    """Test user cannot use models outside their scope."""
    db = MockDB()
    # Bob has no custom config, strict mode = no models
    client = LLMClient(db=db, user_id="bob", use_default_models=False)

    # Should raise - bob has no models in strict mode
    with pytest.raises(PermissionError, match="not available"):
        client._check_model_permission("gpt-4o")


def test_permission_check_tenant_scope():
    """Test tenant scope allows specific models."""
    db = MockDB()
    client = LLMClient(db=db, tenant_id="team_a", use_default_models=False)

    # Should not raise - claude is in tenant scope
    client._check_model_permission("claude-3-5-sonnet-20241022")


def test_api_key_resolution_user_scope():
    """Test API key resolution prioritizes user scope."""
    db = MockDB()

    # Mock fetchone to return user-scoped token
    original_fetchone = db.fetchone

    def mock_fetchone(query, params=None):
        if "tokens" in query and "type = 'llm'" in query:
            return {
                "token_id": "tok_123",
                "provider": "openai",
                "encrypted_value": "user_key_123",
                "secret_ref": None,
            }
        return original_fetchone(query, params)

    db.fetchone = mock_fetchone

    client = LLMClient(db=db, user_id="alice")
    key = client._get_api_key("openai")

    # Should get user-scoped key
    assert key == "user_key_123"


def test_api_key_resolution_fallback_to_env(monkeypatch):
    """Test API key falls back to environment variable."""
    db = MockDB()

    # Mock fetchone to return None (no token in DB)
    original_fetchone = db.fetchone

    def mock_fetchone(query, params=None):
        if "tokens" in query:
            return None
        return original_fetchone(query, params)

    db.fetchone = mock_fetchone

    monkeypatch.setenv("OPENAI_API_KEY", "env_key_456")

    client = LLMClient(db=db, user_id="bob")
    key = client._get_api_key("openai")

    # Should fall back to env
    assert key == "env_key_456"


def test_set_user_context_updates_permissions():
    """Test set_user_context updates model permissions."""
    db = MockDB()
    # Start with strict mode and no user context
    client = LLMClient(db=db, use_default_models=False)

    # Initially no user context - will load global config (1 model)
    initial_models = [m.model_name for m in client.router.list_models()]
    assert len(initial_models) == 1  # Only gpt-4o-mini from global
    assert "gpt-4o" not in initial_models

    # Set user context to alice (alice has 3 models in MockDB)
    client.set_user_context(user_id="alice")

    # Should now have alice's models (3 models from DB)
    alice_models = [m.model_name for m in client.router.list_models()]

    # Alice's config has exactly 3 models
    assert len(alice_models) == 3
    assert "gpt-4o" in alice_models
    assert "gpt-4o-mini" in alice_models
    assert "claude-3-5-sonnet-20241022" in alice_models


def test_detailed_permission_error():
    """Test permission error provides detailed information."""
    db = MockDB()
    client = LLMClient(db=db, user_id="bob", use_default_models=False)

    try:
        client._check_model_permission("gpt-4o")
        raise AssertionError("Should have raised PermissionError")
    except PermissionError as e:
        error_msg = str(e)
        # Should contain user info
        assert "bob" in error_msg
        # Should mention no models configured
        assert "No models configured" in error_msg or "Available models" in error_msg


def test_provider_not_configured_error():
    """Test provider error provides setup instructions."""
    db = MockDB()
    client = LLMClient(db=db, user_id="alice", use_default_models=False)

    from core.llm.models import LLMProvider

    try:
        client._get_provider(LLMProvider.OPENAI)
        # May or may not raise depending on env
    except ValueError as e:
        error_msg = str(e)
        # Should contain setup instructions
        assert "API key" in error_msg
        assert any(word in error_msg for word in ["Database", "Environment", "Config"])
