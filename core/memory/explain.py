"""Backward-compat shim — real implementation in core.memory.tabular.explain."""
from core.memory.tabular.explain import *  # noqa: F401,F403
from core.memory.tabular.explain import (
    CandidateScore, RetrievalStats, ContradictionStats, ObserverStats,
    SandboxStats, GovernanceStats, PipelineStats, MemoryStats, ExplainResult,
)

__all__ = [
    "CandidateScore", "RetrievalStats", "ContradictionStats", "ObserverStats",
    "SandboxStats", "GovernanceStats", "PipelineStats", "MemoryStats", "ExplainResult",
]