"""Factory for creating memory service with pluggable retrieval strategy.

See docs/design/memory/backend-management.md §4.3
"""

from __future__ import annotations

import logging
import os
from typing import TYPE_CHECKING, Any

from core.memory.canonical_storage import CanonicalStorage
from core.memory.service import MemoryService
from core.memory.strategy.registry import StrategyDescriptor, StrategyRegistry

if TYPE_CHECKING:
    from collections.abc import Callable

    from core.db_consumer import DbFactory
    from core.memory.config import MemoryGovernanceConfig

logger = logging.getLogger(__name__)

# ── Global strategy registry ──────────────────────────────────────────

_registry = StrategyRegistry()


def _register_builtins() -> None:
    """Register built-in strategies."""
    def _vector_factory(
        *, db_factory: DbFactory, params: dict | None = None,
        config: Any = None, metrics: Any = None, **kw: Any,
    ) -> Any:
        from core.memory.strategy.vector_v1 import VectorRetrievalStrategy
        return VectorRetrievalStrategy(
            db_factory, params=params, config=config, metrics=metrics,
        )

    def _activation_factory(
        *, db_factory: DbFactory, params: dict | None = None,
        config: Any = None, metrics: Any = None, **kw: Any,
    ) -> Any:
        from core.memory.strategy.activation_v1 import ActivationRetrievalStrategy
        return ActivationRetrievalStrategy(
            db_factory, params=params, config=config, metrics=metrics,
        )

    def _activation_index_factory(
        *, db_factory: DbFactory, params: dict | None = None,
        config: Any = None, **kw: Any,
    ) -> Any:
        from core.memory.strategy.activation_index import ActivationIndexManager
        return ActivationIndexManager(
            db_factory, params=params, config=config,
        )

    _registry.register("vector:v1", _vector_factory)
    _registry.register("activation:v1", _activation_factory, _activation_index_factory)


_register_builtins()


# ── Backward-compatible mapping ───────────────────────────────────────

_BACKEND_TO_STRATEGY: dict[str, str] = {
    "tabular": "vector:v1",
    "graph": "activation:v1",
}


def _resolve_strategy(backend: str | None, strategy: str | None) -> str:
    """Resolve strategy key from backend name or explicit strategy.

    Resolution order:
    1. Explicit strategy parameter
    2. Backend name mapped to strategy
    3. MEM_RETRIEVAL_STRATEGY env var
    4. "vector:v1" hardcoded fallback
    """
    if strategy:
        return strategy
    if backend:
        mapped = _BACKEND_TO_STRATEGY.get(backend)
        if mapped:
            return mapped
        # Treat as strategy key directly (e.g. "vector:v1")
        return backend
    return os.environ.get("MEM_RETRIEVAL_STRATEGY", "vector:v1")


def create_memory_service(
    db_factory: DbFactory,
    *,
    backend: str | None = None,
    strategy: str | None = None,
    llm_client: object | None = None,
    embed_fn: Callable | None = None,
    config: MemoryGovernanceConfig | None = None,
) -> MemoryService:
    """Create memory service with pluggable retrieval strategy.

    Args:
        db_factory: Database session factory.
        backend: Legacy backend name ("tabular" or "graph"). Maps to strategy.
        strategy: Explicit strategy key ("vector:v1", "activation:v1").
        llm_client: LLM client for memory extraction.
        embed_fn: Embedding function.
        config: Governance configuration.

    Returns:
        MemoryService with canonical storage + selected retrieval strategy.
    """
    strategy_key = _resolve_strategy(backend, strategy)

    if config is None:
        from core.memory.config import DEFAULT_CONFIG
        config = DEFAULT_CONFIG

    from core.memory.tabular.metrics import MemoryMetrics
    metrics = MemoryMetrics()

    # Create canonical storage (shared by all strategies)
    storage = CanonicalStorage(
        db_factory,
        llm_client=llm_client,
        embed_fn=embed_fn,
        config=config,
        metrics=metrics,
    )

    # Create retrieval strategy + optional index manager
    descriptor = StrategyDescriptor.parse(strategy_key)
    retrieval = _registry.create_strategy(
        descriptor, db_factory=db_factory, config=config, metrics=metrics,
    )
    index_manager = _registry.create_index_manager(
        descriptor, db_factory=db_factory, config=config,
    )

    return MemoryService(
        storage=storage,
        retrieval=retrieval,
        index_manager=index_manager,
    )
