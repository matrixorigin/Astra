"""Memory types - minimal definitions for Memoria backend."""

from datetime import datetime, timezone
from enum import Enum
from typing import Any, Dict, List, Optional
from dataclasses import dataclass


class MemoryType(str, Enum):
    """Memory type enumeration."""
    SEMANTIC = "semantic"
    PROFILE = "profile" 
    PROCEDURAL = "procedural"
    WORKING = "working"
    TOOL_RESULT = "tool_result"
    EPISODIC = "episodic"


class TrustTier(str, Enum):
    """Trust tier enumeration."""
    T1 = "T1"  # Highest trust
    T2 = "T2"
    T3 = "T3"  # Default
    T4 = "T4"
    T5 = "T5"  # Lowest trust


@dataclass
class Memory:
    """Memory data structure."""
    memory_id: str
    user_id: str
    content: str
    memory_type: MemoryType = MemoryType.SEMANTIC
    trust_tier: TrustTier = TrustTier.T3
    session_id: Optional[str] = None
    source_event_ids: List[str] = None
    embedding: Optional[List[float]] = None
    observed_at: Optional[datetime] = None
    created_at: Optional[datetime] = None
    initial_confidence: float = 0.75  # Add this field
    retrieval_score: Optional[float] = None  # Set by retriever
    
    def __post_init__(self):
        if self.source_event_ids is None:
            self.source_event_ids = []
        if self.observed_at is None:
            self.observed_at = datetime.now(timezone.utc)
        if self.created_at is None:
            self.created_at = datetime.now(timezone.utc)


@dataclass
class RetrievalWeights:
    """Weights for memory retrieval scoring."""
    semantic: float = 1.0
    keyword: float = 0.5
    temporal: float = 0.3
    causal: float = 0.2


def _utcnow() -> datetime:
    """Get current UTC datetime."""
    return datetime.now(timezone.utc)


# Trust tier defaults for compatibility
trust_tier_defaults = {
    "T1": 0.95,
    "T2": 0.85, 
    "T3": 0.75,
    "T4": 0.65,
    "T5": 0.55
}
