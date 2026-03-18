"""Factory and capability registry for memory backends."""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any

_ALL_MEMORY_TOOLS: tuple[str, ...] = (
    "memory_retrieve",
    "memory_search",
    "memory_profile",
    "memory_store",
    "memory_correct",
    "memory_purge",
)

_ALL_CONTEXT_MODES: tuple[str, ...] = ("profile_only", "retrieve", "search")

_TOOL_FALLBACKS: dict[str, tuple[str, ...]] = {
    "memory_profile": ("memory_profile", "memory_retrieve", "memory_search"),
    "memory_retrieve": ("memory_retrieve", "memory_search", "memory_profile"),
    "memory_search": ("memory_search", "memory_retrieve", "memory_profile"),
    "memory_store": ("memory_store",),
    "memory_correct": ("memory_correct", "memory_search", "memory_retrieve"),
    "memory_purge": ("memory_purge", "memory_search", "memory_retrieve"),
}

_MODE_FALLBACKS: dict[str, tuple[str, ...]] = {
    "none": ("none",),
    "profile_only": ("profile_only", "retrieve", "search", "none"),
    "retrieve": ("retrieve", "search", "profile_only", "none"),
    "search": ("search", "retrieve", "profile_only", "none"),
}


@dataclass(frozen=True)
class MemoryBackendCapabilities:
    """Capability profile for a concrete memory backend."""

    backend_name: str
    supported_tools: tuple[str, ...]
    supported_context_modes: tuple[str, ...]
    notes: str = ""

    def supports_tool(self, tool_name: str | None) -> bool:
        return bool(tool_name) and tool_name in self.supported_tools

    def supports_context_mode(self, mode: str | None) -> bool:
        return mode == "none" or (bool(mode) and mode in self.supported_context_modes)

    def resolve_tool(self, tool_name: str | None) -> str | None:
        if not tool_name:
            return None
        for candidate in _TOOL_FALLBACKS.get(tool_name, (tool_name,)):
            if self.supports_tool(candidate):
                return candidate
        return None

    def resolve_context_mode(self, mode: str | None) -> str:
        normalized = (mode or "none").strip().lower() or "none"
        for candidate in _MODE_FALLBACKS.get(normalized, (normalized,)):
            if self.supports_context_mode(candidate):
                return candidate
        return "none"

    def as_dict(self) -> dict[str, object]:
        return {
            "backend_name": self.backend_name,
            "supported_tools": list(self.supported_tools),
            "supported_context_modes": list(self.supported_context_modes),
            "notes": self.notes,
        }


_CAPABILITY_REGISTRY: dict[str, MemoryBackendCapabilities] = {
    "memoria": MemoryBackendCapabilities(
        backend_name="memoria",
        supported_tools=_ALL_MEMORY_TOOLS,
        supported_context_modes=_ALL_CONTEXT_MODES,
        notes="Full Memoria HTTP backend",
    ),
    "memoria_http": MemoryBackendCapabilities(
        backend_name="memoria",
        supported_tools=_ALL_MEMORY_TOOLS,
        supported_context_modes=_ALL_CONTEXT_MODES,
        notes="Alias of Memoria HTTP backend",
    ),
}


def get_memory_backend_name() -> str:
    """Return configured memory backend name."""
    return os.environ.get("MEMORY_BACKEND", "memoria").strip().lower() or "memoria"


def get_memory_backend_capabilities(
    backend_name: str | None = None,
) -> MemoryBackendCapabilities:
    """Return the capability profile for the configured backend."""

    resolved = (backend_name or get_memory_backend_name()).strip().lower()
    if resolved not in _CAPABILITY_REGISTRY:
        raise ValueError(f"Unsupported memory backend: {resolved}")
    return _CAPABILITY_REGISTRY[resolved]


def resolve_memory_tool_name(tool_name: str | None, backend_name: str | None = None) -> str | None:
    """Resolve a memory tool to a backend-supported equivalent, if any."""

    return get_memory_backend_capabilities(backend_name).resolve_tool(tool_name)


def resolve_memory_context_mode(mode: str | None, backend_name: str | None = None) -> str:
    """Resolve a memory context mode to a backend-supported equivalent."""

    return get_memory_backend_capabilities(backend_name).resolve_context_mode(mode)


def create_memory_client(backend_name: str | None = None) -> Any:
    """Create a backend-specific memory client."""

    capabilities = get_memory_backend_capabilities(backend_name)
    if capabilities.backend_name == "memoria":
        from core import config as config_module
        from core.memory.backends.memoria_http import MemoriaHTTPClient

        cfg = config_module.get_memoria_config()
        return MemoriaHTTPClient(
            base_url=cfg.base_url,
            api_key=cfg.api_key,
            master_key=cfg.master_key,
        )
    raise ValueError(f"Unsupported memory backend: {capabilities.backend_name}")


def create_memory_storage(user_id: str, backend_name: str | None = None) -> Any:
    """Create a user-scoped memory storage adapter from configured backend."""

    if not user_id:
        raise ValueError("create_memory_storage requires a non-empty user_id")

    capabilities = get_memory_backend_capabilities(backend_name)
    if capabilities.backend_name == "memoria":
        from core import config as config_module
        from core.memory.backends.memoria_http import MemoriaStorage

        cfg = config_module.get_memoria_config()
        if not cfg.auth_key:
            raise RuntimeError(
                "Memoria requires authentication. Set MEMORIA_MASTER_KEY or MEMORIA_API_KEY."
            )
        if not cfg.master_key and cfg.api_key:
            raise RuntimeError(
                "Memoria requires MEMORIA_MASTER_KEY for multi-user writes. "
                "MEMORIA_API_KEY alone cannot impersonate user_id — data would be written "
                "to the API key's own user, not the intended user_id."
            )
        client = create_memory_client(capabilities.backend_name)
        return MemoriaStorage(client, user_id=user_id)
    raise ValueError(f"Unsupported memory backend: {capabilities.backend_name}")
