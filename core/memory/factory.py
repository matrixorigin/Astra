"""Factory for creating memory service by backend type."""

from __future__ import annotations

from typing import TYPE_CHECKING, Callable, Optional, Union

if TYPE_CHECKING:
    from core.db_consumer import DbFactory
    from core.memory.config import MemoryGovernanceConfig
    from core.memory.interfaces import MemoryAdmin, MemoryReader, MemoryWriter

    EmbedFn = Callable[[str], list[float]]
    MemoryService = Union[MemoryReader, MemoryWriter, MemoryAdmin]


def create_memory_service(
    db_factory: DbFactory,
    *,
    backend: str = "tabular",
    llm_client: Optional[object] = None,
    embed_fn: Optional[EmbedFn] = None,
    config: Optional[MemoryGovernanceConfig] = None,
) -> MemoryService:
    """Create memory service by backend.

    Args:
        backend: "tabular" (flat table) or "graph" (graph-based).
        llm_client: LLM client with ``chat_with_tools()`` method.
        embed_fn: Callable that maps text to embedding vector.
        config: Governance configuration (defaults to env-aware DEFAULT_CONFIG).
    """
    if backend == "graph":
        from core.memory.graph.service import GraphMemoryService
        return GraphMemoryService(
            db_factory, llm_client=llm_client, embed_fn=embed_fn, config=config,
        )

    from core.memory.tabular.service import TabularMemoryService
    return TabularMemoryService(
        db_factory, llm_client=llm_client, embed_fn=embed_fn, config=config,
    )
