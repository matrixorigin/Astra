"""Memory backends — pluggable storage implementations."""

from core.memory.backends.memoria_http import MemoriaHTTPClient, MemoriaStorage

__all__ = ["MemoriaHTTPClient", "MemoriaStorage", "get_memoria_storage"]


def get_memoria_storage(user_id: str) -> MemoriaStorage:
    """Create a MemoriaStorage for the given user from environment config.

    Uses core.config.get_memoria_config() so TEST_MEMORIA_* env vars are
    respected in test environments.
    """
    if not user_id:
        raise ValueError("get_memoria_storage requires a non-empty user_id")
    from core.config import get_memoria_config

    cfg = get_memoria_config()
    if not cfg.auth_key:
        raise RuntimeError(
            "Memoria requires authentication. Set MEMORIA_MASTER_KEY or MEMORIA_API_KEY."
        )
    client = MemoriaHTTPClient(
        base_url=cfg.base_url,
        api_key=cfg.api_key,
        master_key=cfg.master_key,
    )
    return MemoriaStorage(client, user_id=user_id)
