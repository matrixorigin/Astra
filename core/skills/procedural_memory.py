"""Bridge: expose skill_selection_learnings as procedural Memory objects.

This lets the memory retriever and governance system treat skill learnings
as procedural memories without physically moving data into mem_memories.

Storage stays in skill_selection_learnings (indexed, typed columns).
Type unification happens here — every learning can be viewed as a Memory.
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Optional

from sqlalchemy.orm import Session

from api.models import SkillSelectionLearning
from core.memory.types import Memory, MemoryType, TrustTier
from core.skills.learning_similarity import normalize_confidence


def learning_to_memory(row: SkillSelectionLearning) -> Memory:
    """Convert a SkillSelectionLearning row to a Memory domain object."""
    # Build human-readable content from structured fields
    wrong = ", ".join(row.wrong_skills or [])
    correct = ", ".join(row.correct_skills or [])
    parts = [f"[{row.signal_type}] pattern={row.query_pattern!r}"]
    if wrong:
        parts.append(f"wrong=[{wrong}]")
    if correct:
        parts.append(f"correct=[{correct}]")
    content = " ".join(parts)

    return Memory(
        memory_id=row.learning_id,
        user_id="__system__",
        memory_type=MemoryType.PROCEDURAL,
        content=content,
        initial_confidence=normalize_confidence(row.confidence),
        embedding=row.query_embedding if hasattr(row, "query_embedding") else None,
        is_active=bool(row.is_active) if row.is_active is not None else True,
        observed_at=row.created_at,
        created_at=row.created_at,
        updated_at=row.updated_at,
        trust_tier=TrustTier.T3_INFERRED,
    )


def list_as_memories(db: Session, *, active_only: bool = True) -> list[Memory]:
    """Query all learnings and return them as Memory objects."""
    q = db.query(SkillSelectionLearning)
    if active_only:
        q = q.filter(SkillSelectionLearning.is_active == 1)
    return [learning_to_memory(row) for row in q.all()]
