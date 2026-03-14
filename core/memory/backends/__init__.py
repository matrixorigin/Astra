"""Memory backends — pluggable storage implementations."""

import os
from typing import Optional

from core.memory.backends.memoria_http import MemoriaHTTPClient, MemoriaStorage

__all__ = ["MemoriaHTTPClient", "MemoriaStorage", "get_memoria_storage"]


def get_memoria_storage(user_id: str) -> MemoriaStorage:
    """Create a MemoriaStorage for the given user from environment config."""
    base_url = os.environ.get("MEMORIA_BASE_URL", "http://localhost:8000")
    master_key = os.environ.get("MEMORIA_MASTER_KEY")
    api_key = os.environ.get("MEMORIA_API_KEY")
    if not master_key and not api_key:
        raise RuntimeError(
            "Memoria requires authentication. "
            "Set MEMORIA_MASTER_KEY or MEMORIA_API_KEY."
        )
    client = MemoriaHTTPClient(base_url=base_url, master_key=master_key, api_key=api_key)
    return MemoriaStorage(client, user_id=user_id)
