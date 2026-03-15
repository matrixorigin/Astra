"""Memory models - minimal compatibility layer."""

# Re-export from types for compatibility
from core.memory.types import Memory as MemoryRecord


# Dummy classes for compatibility
class GraphEdge:
    pass


class GraphNode:
    pass


class MemoryEditLog:
    pass


class MemoryUserConfig:
    pass


__all__ = ["MemoryRecord", "GraphEdge", "GraphNode", "MemoryEditLog", "MemoryUserConfig"]
