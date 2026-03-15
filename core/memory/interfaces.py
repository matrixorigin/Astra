"""Memory interfaces - minimal definitions for Memoria backend."""

from typing import Any, Dict, List
from dataclasses import dataclass


@dataclass
class GovernanceReport:
    """Governance report from memory system."""
    total_memories: int = 0
    active_memories: int = 0
    quarantined_memories: int = 0
    compressed_memories: int = 0
    actions_taken: List[str] = None
    
    def __post_init__(self):
        if self.actions_taken is None:
            self.actions_taken = []


@dataclass 
class HealthReport:
    """Health report from memory system."""
    status: str = "healthy"
    total_memories: int = 0
    needs_rebuild: bool = False
    details: Dict[str, Any] = None
    
    def __post_init__(self):
        if self.details is None:
            self.details = {}
