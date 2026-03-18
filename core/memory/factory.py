"""Memory factory - backend-aware editor creation."""

from typing import Any

from core.logging_config import get_logger

logger = get_logger(__name__)

_SENTINEL = object()


def create_editor(
    db_factory: Any = None,
    user_id: str | None = None,
    embed_client: Any | None = _SENTINEL,
) -> Any:
    """Create a MemoryEditor with Memoria backend only."""
    from core.memory.editor import MemoryEditor

    # Resolve embed client
    if embed_client is _SENTINEL:
        embed_client = None
        try:
            from core.embedding import get_embedding_client

            embed_client = get_embedding_client()
        except Exception:
            logger.warning(
                "Embedding client not available — memories will be stored without vectors.",
                exc_info=True,
            )

    if not user_id:
        raise ValueError("create_editor requires a non-empty user_id")
    from core.memory.backends import create_memory_storage, get_memory_backend_capabilities

    capabilities = get_memory_backend_capabilities()
    if not capabilities.supports_tool("memory_store"):
        raise RuntimeError(
            f"Configured memory backend '{capabilities.backend_name}' does not support writes"
        )

    storage = create_memory_storage(user_id)
    return MemoryEditor(storage, db_factory, index_manager=None, embed_client=embed_client)
