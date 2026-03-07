"""Backward-compat shim — real implementation in core.memory.tabular.prompts."""
from core.memory.tabular.prompts import *  # noqa: F401,F403
from core.memory.tabular.prompts import OBSERVER_EXTRACTION_PROMPT

__all__ = ["OBSERVER_EXTRACTION_PROMPT"]