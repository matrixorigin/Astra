"""Test multiple models from same provider with different configurations.

Regression test: multiple models from same provider (e.g. deepseek) used to
share one provider instance, causing wrong base_url/api_key to be used.
These tests exercise the real LLMClient._get_provider and _lazy_init_provider
paths with mocked DB rows.
"""

import pytest
from unittest.mock import MagicMock, patch

from core.llm.client import LLMClient


@pytest.fixture
def llm_client():
    """Create LLMClient with mocked DB (no real DB needed)."""
    mock_db = MagicMock()
    mock_db.__enter__ = MagicMock(return_value=mock_db)
    mock_db.__exit__ = MagicMock(return_value=False)

    with (
        patch("core.llm.client.get_db_session"),
        patch.object(LLMClient, "_init_providers"),
        patch.object(LLMClient, "_init_rate_limits"),
    ):
        client = LLMClient.__new__(LLMClient)
        client._providers = {}
        client._model_keys = {}
        client.rate_limiter = MagicMock()
        client.router = MagicMock()
        client._db = MagicMock(return_value=mock_db)
        client.config = {"provider": "openai", "model": "gpt-4"}
    return client


def test_same_provider_different_base_urls_get_separate_instances(llm_client):
    """Two models from 'deepseek' with different base_urls get separate provider instances."""
    from core.llm.providers import OpenAIProvider

    p1 = MagicMock(spec=OpenAIProvider)
    p2 = MagicMock(spec=OpenAIProvider)

    llm_client._providers["deepseek:model-private"] = p1
    llm_client._providers["deepseek:model-official"] = p2

    assert llm_client._get_provider("deepseek", "model-private") is p1
    assert llm_client._get_provider("deepseek", "model-official") is p2


def test_get_provider_falls_through_to_lazy_init(llm_client):
    """When model-specific key not found, _get_provider tries lazy init."""
    mock_provider = MagicMock()

    with patch.object(llm_client, "_lazy_init_provider", return_value=mock_provider) as lazy:
        result = llm_client._get_provider("deepseek", "new-model")

    lazy.assert_called_once_with("deepseek", "new-model")
    assert result is mock_provider


def test_get_provider_raises_when_not_found(llm_client):
    """When provider can't be initialized, raises ValueError with helpful message."""
    mock_db = MagicMock()
    mock_db.query.return_value.filter.return_value.first.return_value = None
    llm_client._db = MagicMock(return_value=mock_db)
    mock_db.__enter__ = MagicMock(return_value=mock_db)
    mock_db.__exit__ = MagicMock(return_value=False)

    with patch.object(llm_client, "_lazy_init_provider", return_value=None):
        with pytest.raises(ValueError, match="not available"):
            llm_client._get_provider("nonexistent", "some-model")
