"""Backward-compat shim — real implementation in core.memory.tabular.store."""
from core.memory.tabular.store import *  # noqa: F401,F403
from core.memory.tabular.store import MemoryStore

__all__ = ["MemoryStore"]