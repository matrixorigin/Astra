"""Embedding package — unified embedding client with pluggable providers."""

from core.embedding.client import EmbeddingClient
from core.embedding.providers import BaseEmbeddingProvider, LocalProvider, MockProvider, OpenAIProvider

__all__ = [
    "EmbeddingClient",
    "BaseEmbeddingProvider",
    "LocalProvider",
    "MockProvider",
    "OpenAIProvider",
]
