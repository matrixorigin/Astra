"""Memory subsystem — typed memory with tiered retrieval.

Public Interface (for external consumers):
- create_memory_service: factory function (preferred)
- MemoryReader, MemoryWriter, MemoryAdmin: Protocol interfaces
- GovernanceReport, HealthReport: result types
- Memory, MemoryType, TrustTier, RetrievalWeights: shared types

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
]
