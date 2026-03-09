"""Embedding package — unified embedding client with pluggable providers."""

from core.embedding.client import EmbeddingClient
from core.embedding.providers import BaseEmbeddingProvider, LocalProvider, MockProvider, OpenAIProvider

__all__ = [
    "EmbeddingClient",
    "BaseEmbeddingProvider",
    "LocalProvider",
    "MockProvider",
    "OpenAIProvider",
    "get_embedding_client",
]

# Process-wide singleton — created once, reused everywhere.
_shared_client: EmbeddingClient | None = None


def get_embedding_client() -> EmbeddingClient:
    """Get or create the process-wide EmbeddingClient singleton.

    Configured from application settings. Fails fast if the configured
    provider is unavailable (e.g., missing API key).
    """
    global _shared_client
    if _shared_client is None:
        import logging

        from config.settings import get_settings

        s = get_settings()
        _shared_client = EmbeddingClient(
            provider=s.embedding_provider,
            model=s.embedding_model,
            dim=s.embedding_dim,
        )
        logging.getLogger(__name__).info(
            "EmbeddingClient: provider=%s, model=%s, dim=%d",
            s.embedding_provider, s.embedding_model, s.embedding_dim,
        )
    return _shared_client
