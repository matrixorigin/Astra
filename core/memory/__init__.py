"""Memory module - Memoria backend only."""

from core.memory.factory import create_editor
from core.memory.backends import get_memoria_storage

__all__ = ["create_editor", "create_memory_service"]


def create_memory_service(db_factory=None, *, user_id: str = "") -> object:
    """Compatibility shim: create a MemoriaStorage for the given user.

    db_factory is accepted but ignored — Memoria is HTTP-based, not DB-based.
    """
    return get_memoria_storage(user_id)
