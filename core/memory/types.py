"""Memory type definitions and data structures."""

from __future__ import annotations

import enum
import math
from dataclasses import dataclass, field
from datetime import datetime
from typing import Optional


class MemoryType(str, enum.Enum):
    PROFILE = "profile"
    SEMANTIC = "semantic"
    PROCEDURAL = "procedural"
    WORKING = "working"
    TOOL_RESULT = "tool_result"


@dataclass
class Memory:
    """In-memory representation of a memory record."""

    memory_id: str
    user_id: str
    memory_type: MemoryType
    content: str
    initial_confidence: float = 0.75
    embedding: Optional[list[float]] = None
    source_event_ids: list[str] = field(default_factory=list)
    superseded_by: Optional[str] = None
    is_active: bool = True
    session_id: Optional[str] = None
    observed_at: Optional[datetime] = None
    created_at: Optional[datetime] = None
    updated_at: Optional[datetime] = None

    def effective_confidence(self, half_life_days: float = 30.0) -> float:
        """Query-time confidence decay. Never mutates stored value."""
        if self.observed_at is None:
            return self.initial_confidence
        age_days = (datetime.utcnow() - self.observed_at).total_seconds() / 86400.0
        return self.initial_confidence * math.exp(-age_days / half_life_days)


@dataclass
class RetrievalWeights:
    """Weights for hybrid retrieval scoring dimensions."""

    vector: float = 0.3
    keyword: float = 0.2
    temporal: float = 0.2
    confidence: float = 0.3

    def __post_init__(self) -> None:
        total = self.vector + self.keyword + self.temporal + self.confidence
        if abs(total - 1.0) > 0.01:
            raise ValueError(f"Weights must sum to 1.0, got {total}")
