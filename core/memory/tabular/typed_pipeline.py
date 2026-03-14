"""Typed memory pipeline: TypedObserver → Persist.

Pipeline phases:
  Phase 1: Observer extracts candidate memories (NOT yet persisted)
  Phase 2: Persist memories (with contradiction check)

Reflector removed — no episodic→semantic promotion (episodic type eliminated).
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from typing import Any, Optional

from core.db_consumer import DbFactory
from core.memory.config import MemoryGovernanceConfig, DEFAULT_CONFIG
from core.memory.tabular.explain import ObserverStats, PipelineStats
from core.memory.tabular.metrics import MemoryMetrics
from core.memory.tabular.store import MemoryStore
from core.memory.tabular.typed_observer import TypedObserver
from core.memory.tabular.profile import ProfileManager
from core.memory.types import Memory

logger = logging.getLogger(__name__)


@dataclass
class TypedPipelineResult:
    memories_extracted: int = 0
    profile_changed: bool = False
    errors: list[str] = field(default_factory=list)
    stats: Optional[PipelineStats] = None


def run_typed_memory_pipeline(
    db_factory: DbFactory,
    user_id: str,
    messages: list[dict[str, Any]],
    source_event_ids: Optional[list[str]] = None,
    llm_client: Any = None,
    embed_fn: Any = None,
    config: Optional[MemoryGovernanceConfig] = None,
    explain: bool = False,
    metrics: Optional[MemoryMetrics] = None,
) -> TypedPipelineResult:
    """Run typed memory pipeline: extract → persist."""
    if config is None:
        config = DEFAULT_CONFIG

    _metrics = metrics or MemoryMetrics()
    start = time.time() if explain else 0
    result = TypedPipelineResult()
    if explain:
        result.stats = PipelineStats()

    store = MemoryStore(db_factory, metrics=_metrics)
    profile_mgr = ProfileManager(store)

    # Phase 1: Observer — extract candidate memories (NOT persisted yet)
    observer_start = time.time() if explain else 0
    candidates: list[Memory] = []
    observer_stats: Optional[ObserverStats] = None
    try:
        observer = TypedObserver(
            store=store,
            llm_client=llm_client,
            embed_fn=embed_fn,
            contradiction_threshold=config.contradiction_similarity_threshold,
            db_factory=db_factory,
            metrics=_metrics,
        )
        candidates = observer.extract_candidates(user_id, messages, source_event_ids)
        result.memories_extracted = len(candidates)
        if explain:
            observer_stats = ObserverStats(memories_extracted=len(candidates))
    except Exception as e:
        logger.error("Typed pipeline observer failed: %s", e)
        result.errors.append(f"observer: {e}")
        return result

    if not candidates:
        if result.stats:
            result.stats.total_ms = (time.time() - start) * 1000
        return result

    # Phase 2: Persist memories (with contradiction check)
    persisted: list[Memory] = []
    for mem in candidates:
        try:
            stored, c_stats = observer.persist_with_contradiction_check(mem, explain)
            persisted.append(stored)
            if observer_stats and c_stats and c_stats.found:
                observer_stats.memories_superseded += 1
                if observer_stats.contradiction is None:
                    observer_stats.contradiction = c_stats
        except Exception as e:
            logger.warning("Failed to persist memory: %s", e)

    if observer_stats:
        observer_stats.memories_stored = len(persisted)
        observer_stats.total_ms = (time.time() - observer_start) * 1000

    result.profile_changed = profile_mgr.update_from_memories(user_id, persisted)

    if result.stats:
        result.stats.observer = observer_stats
        result.stats.total_ms = (time.time() - start) * 1000

    return result
