"""Backward-compat shim — real implementation in core.memory.tabular.health."""
from core.memory.tabular.health import *  # noqa: F401,F403
from core.memory.tabular.health import MemoryHealth

__all__ = ["MemoryHealth"]