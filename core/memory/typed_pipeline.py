"""Typed memory pipeline: TypedObserver → Sandbox → TypedReflector.

Pipeline phases:
  Phase 1: Observer extracts candidate memories (NOT yet persisted)
  Phase 2: Sandbox validates candidates in a zero-copy branch (optional)
  Phase 3: Persist validated memories (rejected candidates are discarded)
  Phase 4: Reflector promotes episodic clusters to semantic
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Optional

from core.db_consumer import DbFactory
from core.memory.config import MemoryGovernanceConfig, DEFAULT_CONFIG
from core.memory.metrics import metrics
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
    """Run typed memory pipeline: extract → validate → persist → reflect.

    Phase 1: Observer extracts candidate memories (returned, NOT stored).
    Phase 2: Sandbox validates candidates in a branch (if query_for_sandbox provided).
             Rejected candidates are discarded; validated ones proceed.
    Phase 3: Persist validated memories via store (with contradiction check).
    Phase 4: Reflector promotes episodic clusters to semantic.
    """
    if config is None:
        config = DEFAULT_CONFIG

    result = TypedPipelineResult()
    store = MemoryStore(db_factory)
    profile_mgr = ProfileManager(store)

    # Phase 1: Observer — extract candidate memories (NOT persisted yet)
    candidates: list[Memory] = []
    try:
        observer = TypedObserver(
            store=store,
            llm_client=llm_client,
            embed_fn=embed_fn,
            contradiction_threshold=config.contradiction_similarity_threshold,
            db_factory=db_factory,
        )
        candidates = observer.extract_candidates(user_id, messages, source_event_ids)
        result.memories_extracted = len(candidates)
    except Exception as e:
        logger.error("Typed pipeline observer failed: %s", e)
        result.errors.append(f"observer: {e}")
        return result

    if not candidates:
        return result

    # Phase 2: Sandbox validation (optional, for configured types)
    validated = candidates  # default: all pass
    if query_for_sandbox:
        validated = []
        try:
            sandbox = MemorySandbox(db_factory)
            # Split into sandbox-eligible and pass-through
            needs_validation = []
            for mem in candidates:
                if mem.memory_type.value in config.sandbox_enabled_types:
                    needs_validation.append(mem)
                else:
                    validated.append(mem)

            if needs_validation:
                passed = sandbox.validate_memories(
                    user_id=user_id,
                    new_memories=needs_validation,
                    query_text=query_for_sandbox,
                    query_embedding=needs_validation[0].embedding,
                )
                if passed:
                    validated.extend(needs_validation)
                    result.memories_validated = len(needs_validation)
                else:
                    result.memories_rejected = len(needs_validation)
                    logger.info(
                        "Sandbox rejected %d memories for user %s",
                        len(needs_validation), user_id,
                    )
            else:
                validated = candidates
        except Exception as e:
            logger.warning("Sandbox validation failed, accepting all: %s", e)
            metrics.increment("sandbox_validation_errors")
            validated = candidates

    # Phase 3: Persist validated memories (with contradiction check)
    persisted: list[Memory] = []
    for mem in validated:
        try:
            stored = observer.persist_with_contradiction_check(mem)
            persisted.append(stored)
        except Exception as e:
            logger.warning("Failed to persist memory: %s", e)

    # Check if profile changed
    result.profile_changed = profile_mgr.update_from_memories(user_id, persisted)

    # Phase 4: Reflector — promote episodic clusters to semantic
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
