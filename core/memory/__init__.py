"""Memory subsystem — typed memory with tiered retrieval.

Public Interface (for external consumers):
- MemoryService: single entry point (facade)
- MemoryReader, MemoryWriter, MemoryAdmin: Protocol interfaces
- GovernanceReport, HealthReport: result types
- parse_json_array: utility for parsing LLM JSON output

Internal Components (use MemoryService instead):
- TypedObserver, MemoryStore, MemoryRetriever, ProfileManager
  Still exported for backward compatibility and tests.
  Production code should use MemoryService.

See docs/design/memory-architecture.md §11 "Module Independence".
"""

# ── Public interface ──────────────────────────────────────────────────
from core.memory.config import DEFAULT_CONFIG, MemoryGovernanceConfig
from core.memory.explain import (
    ContradictionStats,
    ExplainResult,
    GovernanceStats,
    MemoryStats,
    ObserverStats,
    PipelineStats,
    RetrievalStats,
    SandboxStats,
)
from core.memory.interfaces import (
    GovernanceReport,
    HealthReport,
    MemoryAdmin,
    MemoryReader,
    MemoryWriter,
)
from core.memory.json_utils import parse_json_array

# ── Internal components (backward compat — prefer MemoryService) ─────
from core.memory.profile import ProfileManager
from core.memory.retriever import MemoryRetriever
from core.memory.sensitivity import SensitivityResult, check_sensitivity
from core.memory.service import MemoryService
from core.memory.store import MemoryStore
from core.memory.typed_observer import TypedObserver

# Types — shared vocabulary
from core.memory.types import (
    TRUST_TIER_HALF_LIVES,
    TRUST_TIER_INITIAL_CONFIDENCE,
    Memory,
    MemoryType,
    RetrievalWeights,
    TrustTier,
    trust_tier_defaults,
)

__all__ = [
    "DEFAULT_CONFIG",
    "TRUST_TIER_HALF_LIVES",
    "TRUST_TIER_INITIAL_CONFIDENCE",
    "ContradictionStats",
    "ExplainResult",
    "GovernanceReport",
    "GovernanceStats",
    "HealthReport",
    "Memory",
    "MemoryAdmin",
    "MemoryGovernanceConfig",
    "MemoryReader",
    "MemoryRetriever",
    "MemoryService",
    "MemoryStats",
    "MemoryStore",
    "MemoryType",
    "MemoryWriter",
    "ObserverStats",
    "PipelineStats",
    "ProfileManager",
    "RetrievalStats",
    "RetrievalWeights",
    "SandboxStats",
    "SensitivityResult",
    "TrustTier",
    "TypedObserver",
    "check_sensitivity",
    "parse_json_array",
    "trust_tier_defaults",
]
