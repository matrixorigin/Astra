"""Memory Editor - Memoria backend wrapper."""

from typing import Any


class MemoryEditor:
    """Simplified memory editor that wraps Memoria storage."""
    
    def __init__(self, storage: Any, db_factory: Any = None, index_manager: Any = None, embed_client: Any = None):
        self.storage = storage
        self._storage = storage  # Add alias for compatibility
        self.db_factory = db_factory
        self.embed_client = embed_client
    
    def inject(self, content: str, memory_type: str = "semantic", **kwargs) -> dict:
        """Inject memory via Memoria."""
        from core.memory.types import MemoryType
        mem_type = MemoryType(memory_type) if isinstance(memory_type, str) else memory_type
        return self.storage.store(self.storage.user_id, content, memory_type=mem_type, **kwargs)
    
    def correct(self, memory_id: str, new_content: str, reason: str = "") -> dict:
        """Correct memory via Memoria."""
        return self.storage.correct(self.storage.user_id, memory_id, new_content, reason=reason)
    
    def purge(self, memory_id: str = None, topic: str = None, reason: str = "") -> dict:
        """Purge memory via Memoria."""
        if memory_id:
            return self.storage.purge(self.storage.user_id, memory_ids=[memory_id], reason=reason)
        elif topic:
            return self.storage.purge(self.storage.user_id, topic=topic, reason=reason)
        else:
            raise ValueError("Either memory_id or topic must be provided")
    
    def retrieve(self, query: str, top_k: int = 5, **kwargs) -> list:
        """Retrieve memories via Memoria."""
        memories, _ = self.storage.retrieve(self.storage.user_id, query, top_k=top_k, **kwargs)
        return memories
    
    def batch_inject(self, user_id: str, memories: list, **kwargs) -> list:
        """Batch inject memories via Memoria batch API (single HTTP request).

        user_id must match the user_id this editor was created for.
        """
        if user_id != self.storage.user_id:
            raise ValueError(
                f"batch_inject user_id '{user_id}' does not match editor user_id '{self.storage.user_id}'"
            )
        if not memories:
            return []

        # Use the HTTP client's batch_store directly (accepts list of dicts)
        memory_dicts = []
        for m in memories:
            if isinstance(m, dict):
                memory_dicts.append({
                    "content": m.get("content", ""),
                    "memory_type": m.get("memory_type", "semantic"),
                })
            else:
                memory_dicts.append({"content": str(m), "memory_type": "semantic"})

        return self.storage.client.batch_store(user_id, memory_dicts)
