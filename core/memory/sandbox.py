"""Backward-compat shim — real implementation in core.memory.tabular.sandbox."""
from core.memory.tabular.sandbox import *  # noqa: F401,F403
from core.memory.tabular.sandbox import MemorySandbox

__all__ = ["MemorySandbox"]