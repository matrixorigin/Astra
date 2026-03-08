"""EmbeddingClient — unified embedding interface.

Provider is determined by config. No runtime fallback — misconfigured = fail fast.
"""

from core.embedding.providers import (
    BaseEmbeddingProvider,
    LocalProvider,
    MockProvider,
    OpenAIProvider,
)


class EmbeddingClient:
    """Unified embedding client. Hides local vs API difference."""

    def __init__(self, provider: str, model: str, dim: int, **kwargs):
        self._provider: BaseEmbeddingProvider
        if provider == "mock":
            self._provider = MockProvider(dim)
        elif provider == "local":
            self._provider = LocalProvider(model, dim)
        elif provider == "openai":
            self._provider = OpenAIProvider(
                api_key=kwargs.get("api_key", ""),
                model=model,
                dim=dim,
                base_url=kwargs.get("base_url"),
            )
        else:
            raise ValueError(f"Unknown embedding provider: {provider!r}")

    def embed(self, text: str) -> list[float]:
        return self._provider.embed(text)

    def embed_batch(self, texts: list[str]) -> list[list[float]]:
        """Embed multiple texts in one call (native batch if provider supports it)."""
        return self._provider.embed_batch(texts)

    @property
    def dimension(self) -> int:
        return self._provider.dimension()

    @property
    def model_name(self) -> str:
        return self._provider.model_name()
