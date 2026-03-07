"""Memory subsystem — typed memory with tiered retrieval.

Public Interface (for external consumers):
- create_memory_service: factory function (preferred)
- MemoryService: backward-compat alias for TabularMemoryService
- MemoryReader, MemoryWriter, MemoryAdmin: Protocol interfaces
- GovernanceReport, HealthReport: result types

See docs/design/memory/backend-coexistence.md
"""

# ── Public interface ──────────────────────────────────────────────────
from core.memory.config import DEFAULT_CONFIG, MemoryGovernanceConfig
from core.memory.factory import create_memory_service
from core.memory.interfaces import (
    GovernanceReport,
    HealthReport,
    MemoryAdmin,
    MemoryReader,
    MemoryWriter,
)

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

# ── Backward-compat re-exports from tabular backend ──────────────────
from core.memory.tabular.explain import (
    ContradictionStats,
    ExplainResult,
    GovernanceStats,
    MemoryStats,
    ObserverStats,
    PipelineStats,
    RetrievalStats,
    SandboxStats,
)
from core.memory.tabular.json_utils import parse_json_array
from core.memory.tabular.profile import ProfileManager
from core.memory.tabular.retriever import MemoryRetriever
from core.memory.tabular.sensitivity import SensitivityResult, check_sensitivity
from core.memory.tabular.service import TabularMemoryService as MemoryService
from core.memory.tabular.store import MemoryStore
from core.memory.tabular.typed_observer import TypedObserver

__all__ = [
    # Public API
    "create_memory_service",
    # Protocols
    "MemoryReader",
    "MemoryWriter",
    "MemoryAdmin",
    "GovernanceReport",
    "HealthReport",
    # Shared types
    "DEFAULT_CONFIG",
    "Memory",
    "MemoryGovernanceConfig",
    "MemoryType",
    "RetrievalWeights",
    "TrustTier",
    "TRUST_TIER_HALF_LIVES",
    "TRUST_TIER_INITIAL_CONFIDENCE",
    "trust_tier_defaults",
    # Backward-compat aliases
    "MemoryService",
    "MemoryStore",
    "MemoryRetriever",
    "TypedObserver",
    "ProfileManager",
    "parse_json_array",
    "check_sensitivity",
    "SensitivityResult",
    # Explain types
    "ContradictionStats",
    "ExplainResult",
    "GovernanceStats",
    "MemoryStats",
    "ObserverStats",
    "PipelineStats",
    "RetrievalStats",
    "SandboxStats",
]
