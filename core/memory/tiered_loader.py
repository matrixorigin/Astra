"""Tiered memory loader for PromptAssembler §4.

L0: Profile (always loaded, ~200 tokens)
L1: Query-aware retrieval (per-turn, ~800 tokens)
"""

from __future__ import annotations

import logging
from typing import Optional

from core.db_consumer import DbFactory
from core.memory.profile import ProfileManager
from core.memory.retriever import MemoryRetriever
from core.memory.store import MemoryStore
from core.memory.types import MemoryType

logger = logging.getLogger(__name__)


class TieredMemoryLoader:
    """Load L0 (profile) + L1 (query-relevant) memories for prompt §4."""

    def __init__(self, db_factory: DbFactory):
        self._db_factory = db_factory
        self._store: Optional[MemoryStore] = None
        self._profile_mgr: Optional[ProfileManager] = None
        self._retriever: Optional[MemoryRetriever] = None

    def _ensure_initialized(self) -> bool:
        if self._store is None:
            try:
                self._store = MemoryStore(self._db_factory)
                self._profile_mgr = ProfileManager(self._store)
                self._retriever = MemoryRetriever(self._db_factory)
                return True
            except Exception as e:
                logger.debug("TieredMemoryLoader init failed: %s", e)
                return False
        return True

    def load_l0(self, user_id: str) -> str:
        """Load L0 profile (~200 tokens). Cached."""
        if not self._ensure_initialized():
            return ""
        try:
            return self._profile_mgr.get_profile(user_id)
        except Exception as e:
            logger.debug("L0 load failed: %s", e)
            return ""

    def load_l1(
        self,
        user_id: str,
        query: str,
        query_embedding: Optional[list[float]] = None,
        task_hint: Optional[str] = None,
        limit: int = 10,
    ) -> str:
        """Load L1 query-relevant memories (~800 tokens)."""
        if not self._ensure_initialized():
            return ""
        try:
            memories = self._retriever.retrieve(
                user_id=user_id,
                query_text=query,
                query_embedding=query_embedding,
                memory_types=[MemoryType.EPISODIC, MemoryType.SEMANTIC, MemoryType.PROCEDURAL],
                limit=limit,
                task_hint=task_hint,
            )
            if not memories:
                return ""
            lines = ["Relevant Memories:"]
            for m in memories:
                lines.append(f"- [{m.memory_type.value}] {m.content}")
            return "\n".join(lines)
        except Exception as e:
            logger.debug("L1 load failed: %s", e)
            return ""

    def build_section(
        self,
        user_id: str,
        query: str,
        query_embedding: Optional[list[float]] = None,
        task_hint: Optional[str] = None,
    ) -> str:
        """Build complete §4 memory section: L0 + L1."""
        parts = []

        l0 = self.load_l0(user_id)
        if l0:
            parts.append(l0)

        l1 = self.load_l1(user_id, query, query_embedding, task_hint)
        if l1:
            parts.append(l1)

        return "\n\n".join(parts)

    def invalidate_profile(self, user_id: str) -> None:
        """Invalidate L0 cache when profile changes."""
        if self._profile_mgr:
            self._profile_mgr.invalidate(user_id)
