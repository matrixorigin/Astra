"""Backward-compat shim — real implementation in core.memory.tabular.service."""
from core.memory.tabular.service import TabularMemoryService as MemoryService

__all__ = ["MemoryService"]