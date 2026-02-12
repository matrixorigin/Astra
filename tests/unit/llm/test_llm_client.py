"""Unit tests for LLMClient."""

import pytest
from unittest.mock import Mock, patch

from core.llm.client import LLMClient
from core.llm.models import LLMMessage, LLMProvider


@pytest.fixture
def llm_client():
    """Create LLMClient with default config."""
    with patch('core.llm.client.Database'):
        return LLMClient()


class TestLLMClient:
    """Test LLMClient methods."""

    def test_init_success(self, llm_client):
        """Test successful LLMClient initialization."""
        assert llm_client is not None
        assert hasattr(llm_client, 'chat')
        assert hasattr(llm_client, 'chat_with_tools')

    def test_set_user_context(self, llm_client):
        """Test setting user context."""
        user_id = "user123"
        tenant_id = "tenant123"
        scope_context = {"repo": "test-repo"}
        
        llm_client.set_user_context(user_id, tenant_id, scope_context)
        
        assert llm_client.user_id == user_id
        assert llm_client.tenant_id == tenant_id
        assert llm_client.scope_context == scope_context

    def test_resolve_model_default(self, llm_client):
        """Test model resolution with default."""
        # Mock config
        llm_client.config = {"model": "gpt-4"}
        
        result = llm_client._resolve_model(None)
        assert result == "gpt-4"

    def test_resolve_model_explicit(self, llm_client):
        """Test model resolution with explicit model."""
        result = llm_client._resolve_model("gpt-3.5-turbo")
        assert result == "gpt-3.5-turbo"

    def test_normalize_messages_dict_input(self):
        """Test message normalization with dict input."""
        messages = [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": "Hi there!"}
        ]
        
        result = LLMClient._normalize_messages(messages)
        
        assert result == messages
        assert len(result) == 2
        assert result[0]["role"] == "user"
        assert result[1]["role"] == "assistant"

    def test_normalize_messages_llm_message_input(self):
        """Test message normalization with LLMMessage input."""
        messages = [
            LLMMessage(role="user", content="Hello"),
            LLMMessage(role="assistant", content="Hi there!")
        ]
        
        result = LLMClient._normalize_messages(messages)
        
        assert len(result) == 2
        assert result[0]["role"] == "user"
        assert result[0]["content"] == "Hello"
        assert result[1]["role"] == "assistant"
        assert result[1]["content"] == "Hi there!"

    def test_total_spend_property_default(self, llm_client):
        """Test total spend property with default value."""
        # Mock empty config
        llm_client.config = {}
        
        result = llm_client.total_spend
        
        assert result == 0.0

    def test_get_provider_error_message_fix(self, llm_client):
        """Test that _get_provider error message uses correct string formatting."""
        # This tests the bug fix where p.value was incorrectly used
        llm_client.user_id = "test_user"
        llm_client._providers = {}  # Empty providers to trigger error
        
        with pytest.raises(ValueError) as exc_info:
            llm_client._get_provider(LLMProvider.OPENAI)
        
        error_msg = str(exc_info.value)
        # Should contain "openai" (the value) not "LLMProvider.OPENAI" (the repr)
        assert "openai" in error_msg
        assert "not configured" in error_msg
        assert "test_user" in error_msg
        # Should NOT contain the enum representation
        assert "LLMProvider.OPENAI" not in error_msg
