"""Tests for context overflow detection."""

import pytest
from unittest.mock import MagicMock, patch

from core.llm.client import ContextOverflowError, LLMClient
from core.llm.router import ModelConfig


class TestContextOverflowDetection:
    """Test _check_context_overflow method."""

    @pytest.fixture
    def mock_llm_client(self, db_factory):
        """Create LLMClient with mocked router (8K context window)."""
        with patch.object(LLMClient, "_init_providers"):
            client = LLMClient(db_factory)
            mock_model = ModelConfig(
                model_name="test-model", provider="openai", context_window=8000
            )
            mock_router = MagicMock()
            mock_router.list_models.return_value = [mock_model]
            client.router = mock_router
            return client

    def test_small_messages_pass(self, mock_llm_client):
        """Messages within context window should pass."""
        messages = [{"role": "user", "content": "Hello!"}]
        mock_llm_client._check_context_overflow("test-model", messages)

    def test_large_messages_raise_error(self, mock_llm_client):
        """Messages exceeding context window should raise ContextOverflowError."""
        # 8K context, 1K reserved for response = 7K max prompt
        # 7K tokens * 3 chars/token = 21K chars
        # Use 30K chars to definitely exceed
        messages = [{"role": "system", "content": "x" * 30000}]
        with pytest.raises(ContextOverflowError) as exc_info:
            mock_llm_client._check_context_overflow("test-model", messages)
        assert "Context overflow" in str(exc_info.value)

    def test_empty_messages_pass(self, mock_llm_client):
        """Empty messages should pass."""
        mock_llm_client._check_context_overflow("test-model", [])
        mock_llm_client._check_context_overflow("test-model", None)

    def test_error_suggests_new_session(self, mock_llm_client):
        """Error message should suggest /session new."""
        messages = [{"role": "user", "content": "x" * 30000}]
        with pytest.raises(ContextOverflowError) as exc_info:
            mock_llm_client._check_context_overflow("test-model", messages)
        assert "/session new" in str(exc_info.value)

    def test_boundary_just_under_limit_passes(self, mock_llm_client):
        """Messages just under the limit should pass."""
        # 8K context - 1K reserved = 7K max prompt tokens
        # 7K tokens * 3 chars/token = 21K chars
        # Use 20K chars (safely under)
        messages = [{"role": "user", "content": "x" * 20000}]
        mock_llm_client._check_context_overflow("test-model", messages)

    def test_boundary_just_over_limit_fails(self, mock_llm_client):
        """Messages just over the limit should fail."""
        # 8K context - 1K reserved = 7K max prompt tokens
        # 7K tokens * 3 chars/token = 21K chars
        # Use 22K chars (just over)
        messages = [{"role": "user", "content": "x" * 22000}]
        with pytest.raises(ContextOverflowError):
            mock_llm_client._check_context_overflow("test-model", messages)

    def test_tool_calls_counted_in_estimate(self, mock_llm_client):
        """Tool calls JSON should be included in token estimate."""
        # Small content but large tool_calls
        large_tool_call = {"id": "tc1", "function": {"name": "test", "arguments": "x" * 25000}}
        messages = [{"role": "assistant", "content": "", "tool_calls": [large_tool_call]}]
        with pytest.raises(ContextOverflowError):
            mock_llm_client._check_context_overflow("test-model", messages)

    def test_unknown_model_uses_default_128k(self, db_factory):
        """Unknown model should use default 128K context window."""
        with patch.object(LLMClient, "_init_providers"):
            client = LLMClient(db_factory)
            mock_router = MagicMock()
            mock_router.list_models.return_value = []  # No models
            client.router = mock_router

            # 128K - 1K = 127K max tokens = ~381K chars
            # 300K chars should pass with 128K default
            messages = [{"role": "user", "content": "x" * 300000}]
            client._check_context_overflow("unknown-model", messages)

            # 500K chars should fail even with 128K default
            messages = [{"role": "user", "content": "x" * 500000}]
            with pytest.raises(ContextOverflowError):
                client._check_context_overflow("unknown-model", messages)
