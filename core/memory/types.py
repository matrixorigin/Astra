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


class TrustTier(str, enum.Enum):
    T1_VERIFIED = "T1"
    T2_CURATED = "T2"
    T3_INFERRED = "T3"
    T4_UNVERIFIED = "T4"


# Half-life in days per trust tier
TRUST_TIER_HALF_LIVES: dict[TrustTier, float] = {
    TrustTier.T1_VERIFIED: 365.0,
    TrustTier.T2_CURATED: 180.0,
    TrustTier.T3_INFERRED: 60.0,
    TrustTier.T4_UNVERIFIED: 30.0,
}

# Initial confidence per trust tier (from architecture §1)
TRUST_TIER_INITIAL_CONFIDENCE: dict[TrustTier, float] = {
    TrustTier.T1_VERIFIED: 0.95,
    TrustTier.T2_CURATED: 0.85,
    TrustTier.T3_INFERRED: 0.65,
    TrustTier.T4_UNVERIFIED: 0.40,
}


def trust_tier_defaults(tier: str) -> dict[str, float]:
    """Return initial_confidence and half_life_days for a trust tier string."""
    try:
        t = TrustTier(tier)
    except ValueError:
        t = TrustTier.T3_INFERRED
    return {
        "initial_confidence": TRUST_TIER_INITIAL_CONFIDENCE[t],
        "half_life_days": TRUST_TIER_HALF_LIVES[t],
    }


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
    trust_tier: TrustTier = TrustTier.T3_INFERRED

    def effective_confidence(self, half_life_days: Optional[float] = None) -> float:
        """Query-time confidence decay. Uses tier-specific half-life if not overridden."""
        if self.observed_at is None:
            return self.initial_confidence
        if half_life_days is None:
            half_life_days = TRUST_TIER_HALF_LIVES.get(self.trust_tier, 60.0)
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
