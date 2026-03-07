"""Backward-compat shim — real implementation in core.memory.tabular.retriever."""
from core.memory.tabular.retriever import *  # noqa: F401,F403
from core.memory.tabular.retriever import MemoryRetriever, TASK_WEIGHTS, _Candidate, _safe_exp

__all__ = ["MemoryRetriever", "TASK_WEIGHTS"]