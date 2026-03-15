"""Memory factory - Memoria backend only."""

import os
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

    # Create Memoria HTTP client
    memoria_url = os.environ.get("MEMORIA_BASE_URL")
    memoria_master_key = os.environ.get("MEMORIA_MASTER_KEY")
    memoria_api_key = os.environ.get("MEMORIA_API_KEY")

    if not memoria_url or not (memoria_master_key or memoria_api_key):
        raise RuntimeError(
            "Memoria is required. Set MEMORIA_BASE_URL and MEMORIA_MASTER_KEY "
            "(or MEMORIA_API_KEY) environment variables."
        )

    from core.memory.backends.memoria_http import MemoriaHTTPClient, MemoriaStorage

    http_client = MemoriaHTTPClient(
        base_url=memoria_url,
        api_key=memoria_api_key,
        master_key=memoria_master_key,
    )

    storage = MemoriaStorage(http_client, user_id=user_id or "default")
    return MemoryEditor(storage, db_factory, index_manager=None, embed_client=embed_client)
