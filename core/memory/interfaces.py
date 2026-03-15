"""Memory interfaces - minimal definitions for Memoria backend."""

from dataclasses import dataclass, field
from typing import Any, Dict, List


@dataclass
class GovernanceReport:
    """Governance report from memory system."""

    total_memories: int = 0
    active_memories: int = 0
    quarantined_memories: int = 0
    compressed_memories: int = 0
    actions_taken: List[str] = field(default_factory=list)


@dataclass
class HealthReport:
    """Health report from memory system."""

    status: str = "healthy"
    total_memories: int = 0
    needs_rebuild: bool = False
    details: Dict[str, Any] = field(default_factory=dict)
