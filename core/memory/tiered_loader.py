"""Tiered memory loader for PromptAssembler §4.

L0: Profile (always loaded, ~200 tokens)
L1: Query-aware retrieval (per-turn, ~800 tokens)

Supports explain=True for EXPLAIN ANALYZE style execution stats.
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass
from typing import Optional

from core.db_consumer import DbFactory
from core.memory.explain import RetrievalStats
from core.memory.metrics import metrics
from core.memory.profile import ProfileManager
from core.memory.retriever import MemoryRetriever
from core.memory.store import MemoryStore
from core.memory.types import MemoryType

logger = logging.getLogger(__name__)


@dataclass
class TieredLoaderStats:
    """Stats for tiered memory loading."""
    l0_loaded: bool = False
    l0_tokens: int = 0
    l0_ms: float = 0.0
    l1_loaded: bool = False
    l1_count: int = 0
    l1_tokens: int = 0
    l1_ms: float = 0.0
    retrieval: Optional[RetrievalStats] = None
    total_ms: float = 0.0


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
                metrics.increment("tiered_loader_init_errors")
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
            metrics.increment("tiered_loader_l0_errors")
            return ""

    def load_l1(
        self,
        user_id: str,
        session_id: str,
        query: str,
        query_embedding: Optional[list[float]] = None,
        task_hint: Optional[str] = None,
        limit: int = 10,
        explain: bool = False,
    ) -> tuple[str, Optional[RetrievalStats]]:
        """Load L1 query-relevant memories (~800 tokens).
        
        Returns:
            (text, stats) — stats is None when explain=False or on error.
        """
        if not self._ensure_initialized():
            return ("", None)
        try:
            memories, stats = self._retriever.retrieve(
                user_id=user_id,
                query_text=query,
                session_id=session_id,
                query_embedding=query_embedding,
                memory_types=[MemoryType.EPISODIC, MemoryType.SEMANTIC, MemoryType.PROCEDURAL],
                limit=limit,
                task_hint=task_hint,
                explain=explain,
            )
            if not memories:
                return "", stats
            lines = ["Relevant Memories:"]
            for m in memories:
                lines.append(f"- [{m.memory_type.value}] {m.content}")
            return "\n".join(lines), stats
        except Exception as e:
            logger.debug("L1 load failed: %s", e)
            metrics.increment("tiered_loader_l1_errors")
            return "", None

    def build_section(
        self,
        user_id: str,
        session_id: str,
        query: str,
        query_embedding: Optional[list[float]] = None,
        task_hint: Optional[str] = None,
        explain: bool = False,
    ) -> tuple[str, Optional[TieredLoaderStats]]:
        """Build complete §4 memory section: L0 + L1.
        
        Returns:
            (text, stats) — stats is None when explain=False.
        """
        start = time.time() if explain else 0
        stats = TieredLoaderStats() if explain else None
        parts = []

        # L0
        l0_start = time.time() if explain else 0
        l0 = self.load_l0(user_id)
        if l0:
            parts.append(l0)
        if stats:
            stats.l0_loaded = bool(l0)
            stats.l0_tokens = len(l0.split()) if l0 else 0  # rough estimate
            stats.l0_ms = (time.time() - l0_start) * 1000

        # L1
        l1_start = time.time() if explain else 0
        l1, retrieval_stats = self.load_l1(user_id, session_id, query, query_embedding, task_hint, explain=explain)
        if l1:
            parts.append(l1)
        if stats:
            stats.l1_loaded = bool(l1)
            stats.l1_count = retrieval_stats.final_count if retrieval_stats else 0
            stats.l1_tokens = len(l1.split()) if l1 else 0
            stats.l1_ms = (time.time() - l1_start) * 1000
            stats.retrieval = retrieval_stats
            stats.total_ms = (time.time() - start) * 1000

        return "\n\n".join(parts), stats

    def invalidate_profile(self, user_id: str) -> None:
        """Invalidate L0 cache when profile changes."""
        if self._profile_mgr:
            self._profile_mgr.invalidate(user_id)
