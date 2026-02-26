"""Memory type definitions and data structures."""

from __future__ import annotations

import enum
from dataclasses import dataclass, field
from datetime import datetime
from typing import Optional


class MemoryType(str, enum.Enum):
    PROFILE = "profile"
    EPISODIC = "episodic"
    SEMANTIC = "semantic"
    PROCEDURAL = "procedural"
    WORKING = "working"


@dataclass
class Memory:
    """In-memory representation of a memory record."""

    memory_id: str
    user_id: str
    memory_type: MemoryType
    content: str
    confidence: float = 0.75
    embedding: Optional[list[float]] = None
    source_event_ids: list[str] = field(default_factory=list)
    superseded_by: Optional[str] = None
    is_active: bool = True
    observed_at: Optional[datetime] = None
    created_at: Optional[datetime] = None
    updated_at: Optional[datetime] = None


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
