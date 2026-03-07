"""Backward-compat shim — real implementation in core.memory.tabular.metrics."""
from core.memory.tabular.metrics import *  # noqa: F401,F403
from core.memory.tabular.metrics import MemoryMetrics, Timer

__all__ = ["MemoryMetrics", "Timer"]