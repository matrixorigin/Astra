"""Retrieval strategy subsystem — pluggable retrieval for memory."""

from core.memory.strategy.protocol import IndexManager, RetrievalStrategy
from core.memory.strategy.registry import StrategyDescriptor, StrategyRegistry

__all__ = [
    "IndexManager",
    "RetrievalStrategy",
    "StrategyDescriptor",
    "StrategyRegistry",
]
