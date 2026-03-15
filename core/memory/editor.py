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
        return self.storage.correct(memory_id, new_content, reason)
    
    def purge(self, memory_id: str = None, topic: str = None, reason: str = "") -> dict:
        """Purge memory via Memoria."""
        if memory_id:
            return self.storage.purge_by_id(memory_id, reason)
        elif topic:
            return self.storage.purge_by_topic(topic, reason)
        else:
            raise ValueError("Either memory_id or topic must be provided")
    
    def retrieve(self, query: str, top_k: int = 5, **kwargs) -> list:
        """Retrieve memories via Memoria."""
        return self.storage.retrieve(query, top_k=top_k, **kwargs)
    
    def batch_inject(self, user_id: str, memories: list, **kwargs) -> list:
        """Batch inject memories via Memoria."""
        results = []
        for memory in memories:
            if isinstance(memory, dict):
                result = self.inject(memory.get("content", ""), memory.get("memory_type", "semantic"), **kwargs)
            else:
                result = self.inject(str(memory), "semantic", **kwargs)
            results.append(result)
        return results
