"""Memory subsystem — typed memory with tiered retrieval.

New architecture (v2):
- TypedObserver: extracts typed memories (profile/episodic/semantic/procedural)
- TypedReflector: promotes episodic clusters to semantic
- MemoryStore: CRUD with atomic contradiction resolution
- MemoryRetriever: query-aware hybrid retrieval
- ProfileManager: L0 profile synthesis
- MemorySandbox: write-ahead validation
- MemoryProvenance: PITR queries and rollback
- MemoryHealth: pollution detection

Legacy (v1, to be removed in Task 10):
- Observer: extracts untyped Observations
- Reflector: condenses Observations
"""

from core.memory.types import Memory, MemoryType, RetrievalWeights
from core.memory.store import MemoryStore
from core.memory.retriever import MemoryRetriever
from core.memory.typed_observer import TypedObserver
from core.memory.typed_reflector import TypedReflector
from core.memory.profile import ProfileManager
from core.memory.config import MemoryGovernanceConfig, DEFAULT_CONFIG

# Legacy exports (backward compatibility)
from core.memory.observer import Observer
from core.memory.reflector import Reflector

__all__ = [
    # Types
    "Memory", "MemoryType", "RetrievalWeights",
    # Core
    "MemoryStore", "MemoryRetriever", "TypedObserver", "TypedReflector",
    "ProfileManager", "MemoryGovernanceConfig", "DEFAULT_CONFIG",
    # Legacy
    "Observer", "Reflector",
]
