"""Factory for creating memory service by backend type."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from core.db_consumer import DbFactory


def create_memory_service(
    db_factory: DbFactory,
    *,
    backend: str = "tabular",
    llm_client: Any = None,
    embed_fn: Any = None,
    config: Any = None,
) -> Any:
    """Create memory service by backend.

    Args:
        backend: "tabular" (flat table) or "graph" (graph-based).
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
