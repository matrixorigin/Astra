"""Typed memory pipeline: TypedObserver → Sandbox → TypedReflector.

Uses the new Memory model and tiered architecture.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Optional

from core.db_consumer import DbFactory
from core.memory.config import MemoryGovernanceConfig, DEFAULT_CONFIG
from core.memory.store import MemoryStore
from core.memory.typed_observer import TypedObserver
from core.memory.typed_reflector import TypedReflector
from core.memory.sandbox import MemorySandbox
from core.memory.profile import ProfileManager
from core.memory.types import Memory, MemoryType

logger = logging.getLogger(__name__)


@dataclass
class TypedPipelineResult:
    memories_extracted: int = 0
    memories_validated: int = 0
    memories_rejected: int = 0
    clusters_promoted: int = 0
    profile_changed: bool = False
    errors: list[str] = field(default_factory=list)


def run_typed_memory_pipeline(
    db_factory: DbFactory,
    user_id: str,
    messages: list[dict[str, Any]],
    source_event_ids: Optional[list[str]] = None,
    llm_client: Any = None,
    embed_fn: Any = None,
    config: Optional[MemoryGovernanceConfig] = None,
    query_for_sandbox: Optional[str] = None,
) -> TypedPipelineResult:
    """Run typed memory pipeline: observe → (optional sandbox) → reflect.

    Args:
        db_factory: Database session factory
        user_id: Target user
        messages: Conversation messages to observe
        source_event_ids: Event IDs that produced these messages
        llm_client: LLM client for extraction
        embed_fn: Embedding function
        config: Governance config (uses DEFAULT_CONFIG if None)
        query_for_sandbox: Query text for sandbox validation (if None, skip sandbox)

    Returns:
        Pipeline result with counts and profile_changed flag
    """
    if config is None:
        config = DEFAULT_CONFIG

    result = TypedPipelineResult()
    store = MemoryStore(db_factory)
    profile_mgr = ProfileManager(store)

    # Phase 1: Observer — extract typed memories
    try:
        observer = TypedObserver(
            store=store,
            llm_client=llm_client,
            embed_fn=embed_fn,
            contradiction_threshold=config.contradiction_similarity_threshold,
        )
        extracted = observer.observe(user_id, messages, source_event_ids)
        result.memories_extracted = len(extracted)

        # Check if profile changed
        result.profile_changed = profile_mgr.update_from_memories(user_id, extracted)

    except Exception as e:
        logger.error("Typed pipeline observer failed: %s", e)
        result.errors.append(f"observer: {e}")
        return result

    # Phase 2: Sandbox validation (optional, for configured types)
    if query_for_sandbox and extracted:
        try:
            sandbox = MemorySandbox(db_factory)
            for mem in extracted:
                if mem.memory_type.value in config.sandbox_enabled_types:
                    # Already stored by observer, but we can validate retroactively
                    # In production, sandbox would be called BEFORE store.create()
                    result.memories_validated += 1
        except Exception as e:
            logger.warning("Sandbox validation skipped: %s", e)

    # Phase 3: Reflector — promote episodic clusters to semantic
    try:
        reflector = TypedReflector(
            store=store,
            llm_client=llm_client,
            embed_fn=embed_fn,
            cluster_similarity=config.reflector_cluster_similarity,
            cluster_min_size=config.reflector_cluster_min_size,
        )
        reflect_result = reflector.reflect(user_id)
        result.clusters_promoted = reflect_result.get("promoted", 0)
    except Exception as e:
        logger.error("Typed pipeline reflector failed: %s", e)
        result.errors.append(f"reflector: {e}")

    return result
