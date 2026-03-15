"""Tiered memory loader for PromptAssembler §4.

Moved from core/memory/ to core/context/ — this is a context-layer consumption
strategy, not a memory-internal component. See memory-architecture.md §11.

L0: Profile (always loaded, ~200 tokens)
L1: Query-aware retrieval (per-turn, ~800 tokens)

Consumes memory through MemoryService (Protocol-based interface).
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from core.memory.types import MemoryType

if TYPE_CHECKING:
    pass

logger = logging.getLogger(__name__)


class MemoryMetrics:
    """Simple metrics counter."""
    def __init__(self):
        self._counters = {}
    
    def increment(self, key: str, value: int = 1):
        self._counters[key] = self._counters.get(key, 0) + value


@dataclass
class TieredLoaderStats:
    l0_loaded: bool = False
    l0_tokens: int = 0
    l0_ms: float = 0.0
    l1_loaded: bool = False
    l1_count: int = 0
    l1_tokens: int = 0
    l1_ms: float = 0.0
    l1_error: bool = False
    retrieval: dict | None = None  # Simplified from RetrievalStats
    total_ms: float = 0.0


class TieredMemoryLoader:
    """Load L0 (profile) + L1 (query-relevant) memories for prompt §4.

    Uses MemoryService as the sole interface to the memory module.
    """

    def __init__(self, memory_service: Any = None, metrics: MemoryMetrics | None = None):
        """Initialize with optional memory_service (legacy) or use Memoria HTTP."""
        self._svc = memory_service
        self._metrics = metrics or MemoryMetrics()
        self._memoria_client = None

        # If no memory_service provided, use Memoria HTTP client
        if self._svc is None:
            try:
                from core.config import get_memoria_config
                cfg = get_memoria_config()
                if cfg.base_url and cfg.auth_key:
                    from core.memory.backends.memoria_http import MemoriaHTTPClient
                    self._memoria_client = MemoriaHTTPClient(
                        base_url=cfg.base_url,
                        api_key=cfg.api_key,
                        master_key=cfg.master_key,
                    )
            except Exception:
                pass  # Memoria not configured — tiered loader will return empty

    def load_l0(self, user_id: str) -> str:
        """Load profile memories. Returns raw content without section header."""
        try:
            if self._memoria_client:
                # Prefer synthesized profile (get_profile) over raw list
                data = self._memoria_client.get_profile(user_id)
                profile = data.get("profile") if isinstance(data, dict) else None
                if profile:
                    return profile
                # Fallback: raw profile memories list
                result = self._memoria_client.list_memories(
                    user_id=user_id,
                    memory_type="profile",
                    limit=10,
                )
                memories = result.get("items", []) if isinstance(result, dict) else result
                if memories:
                    return "\n".join(f"- {m['content']}" for m in memories)
                return ""
            elif self._svc:
                return self._svc.get_profile(user_id) or ""
            return ""
        except Exception as e:
            # L0 is best-effort: any failure (network, auth, parse) degrades gracefully.
            logger.warning("L0 load failed for user %s: %s", user_id, e)
            self._metrics.increment("tiered_loader_l0_errors")
            return ""

    def load_l1(
        self,
        user_id: str,
        session_id: str,
        query: str,
        query_embedding: list[float] | None = None,
        task_hint: str | None = None,
        limit: int = 10,
        explain: bool = False,
    ) -> tuple[str, dict | None]:
        """Load L1 semantic/procedural memories."""
        try:
            if self._memoria_client:
                result = self._memoria_client.retrieve(
                    user_id=user_id,
                    query=query,
                    top_k=limit,
                    memory_types=["semantic", "procedural", "episodic"],
                    session_id=session_id or None,
                )
                memories = result.get("results", result.get("memories", [])) if isinstance(result, dict) else result
                if not memories:
                    return "", {"source": "memoria", "final_count": 0}
                lines = ["Relevant Memories:"]
                for m in memories:
                    lines.append(f"- [{m.get('memory_type', 'semantic')}] {m['content']}")
                return "\n".join(lines), {"source": "memoria", "final_count": len(memories)}
            elif self._svc:
                # Legacy memory service
                memories, stats = self._svc.retrieve(
                    user_id=user_id,
                    query=query,
                    session_id=session_id,
                    query_embedding=query_embedding,
                    memory_types=[MemoryType.SEMANTIC, MemoryType.PROCEDURAL, MemoryType.EPISODIC],
                    top_k=limit,
                    task_hint=task_hint,
                    explain=explain,
                )
                if not memories:
                    return "", stats
                lines = ["Relevant Memories:"]
                for m in memories:
                    lines.append(f"- [{m.memory_type.value}] {m.content}")
                return "\n".join(lines), stats
            return "", None
        except Exception as e:
            # L1 is best-effort: any failure (network, auth, parse) degrades gracefully.
            logger.warning("L1 load failed for user %s: %s", user_id, e)
            self._metrics.increment("tiered_loader_l1_errors")
            return "", None

    def build_section(
        self,
        user_id: str,
        session_id: str,
        query: str,
        query_embedding: list[float] | None = None,
        task_hint: str | None = None,
        explain: bool = False,
    ) -> tuple[str, TieredLoaderStats | None]:
        start = time.time() if explain else 0
        stats = TieredLoaderStats() if explain else None
        parts = []

        l0_start = time.time() if explain else 0
        l0 = self.load_l0(user_id)
        if l0:
            parts.append(l0)
        if stats:
            stats.l0_loaded = bool(l0)
            stats.l0_tokens = len(l0.split()) if l0 else 0
            stats.l0_ms = (time.time() - l0_start) * 1000

        l1_start = time.time() if explain else 0
        l1, retrieval_stats = self.load_l1(
            user_id, session_id, query, query_embedding, task_hint, explain=explain
        )
        if l1:
            parts.append(l1)
        if stats:
            stats.l1_loaded = bool(l1)
            stats.l1_count = len(l1.split("\n")) - 1 if l1 else 0
            stats.l1_tokens = len(l1.split()) if l1 else 0
            stats.l1_ms = (time.time() - l1_start) * 1000
            stats.l1_error = not l1 and retrieval_stats is None
            stats.retrieval = retrieval_stats
            stats.total_ms = (time.time() - start) * 1000

        # Return raw content — caller is responsible for section header
        return "\n\n".join(parts), stats

    def invalidate_profile(self, user_id: str) -> None:
        if self._svc is not None:
            self._svc.invalidate_profile(user_id)
        if self._memoria_client is not None and hasattr(self._memoria_client, "invalidate_profile"):
            self._memoria_client.invalidate_profile(user_id)
