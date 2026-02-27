"""Memory subsystem — typed memory with tiered retrieval.

Components:
- TypedObserver: extracts typed memories (profile/semantic/procedural/working/tool_result)
- MemoryStore: CRUD with atomic contradiction resolution
- MemoryRetriever: query-aware hybrid retrieval
- ProfileManager: L0 profile synthesis
- MemorySandbox: write-ahead validation
- MemoryProvenance: PITR queries and rollback
- MemoryHealth: pollution detection
- SensitivityFilter: PII/credential blocking

All components support explain=True for EXPLAIN ANALYZE style execution stats.
"""

from core.memory.types import Memory, MemoryType, RetrievalWeights, TrustTier, TRUST_TIER_HALF_LIVES, TRUST_TIER_INITIAL_CONFIDENCE, trust_tier_defaults
from core.memory.store import MemoryStore
from core.memory.retriever import MemoryRetriever
from core.memory.typed_observer import TypedObserver
from core.memory.profile import ProfileManager
from core.memory.config import MemoryGovernanceConfig, DEFAULT_CONFIG
from core.memory.sensitivity import check_sensitivity, SensitivityResult
from core.memory.explain import (
    RetrievalStats, ContradictionStats, ObserverStats,
    SandboxStats, GovernanceStats, PipelineStats,
    MemoryStats, ExplainResult,
)

__all__ = [
    "Memory", "MemoryType", "RetrievalWeights", "TrustTier", "TRUST_TIER_HALF_LIVES",
    "TRUST_TIER_INITIAL_CONFIDENCE", "trust_tier_defaults",
    "MemoryStore", "MemoryRetriever", "TypedObserver",
    "ProfileManager", "MemoryGovernanceConfig", "DEFAULT_CONFIG",
    "check_sensitivity", "SensitivityResult",
    # Explain stats
    "RetrievalStats", "ContradictionStats", "ObserverStats",
    "SandboxStats", "GovernanceStats", "PipelineStats",
    "MemoryStats", "ExplainResult",
]
