"""Memory backends — pluggable storage implementations."""

from core.memory.backends.factory import (
    MemoryBackendCapabilities,
    create_memory_client,
    create_memory_storage,
    get_memory_backend_capabilities,
    get_memory_backend_name,
    resolve_memory_context_mode,
    resolve_memory_tool_name,
)
from core.memory.backends.memoria_http import MemoriaHTTPClient, MemoriaStorage

__all__ = [
    "MemoryBackendCapabilities",
    "MemoriaHTTPClient",
    "MemoriaStorage",
    "create_memory_client",
    "create_memory_storage",
    "get_memory_backend_capabilities",
    "get_memory_backend_name",
    "get_memoria_storage",
    "resolve_memory_context_mode",
    "resolve_memory_tool_name",
]


def get_memoria_storage(user_id: str) -> MemoriaStorage:
    """Compatibility shim for the configured memory backend."""
    return create_memory_storage(user_id)
