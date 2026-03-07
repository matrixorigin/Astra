"""Backward-compat shim — real implementation in core.memory.tabular.json_utils."""
from core.memory.tabular.json_utils import *  # noqa: F401,F403
from core.memory.tabular.json_utils import parse_json_array

__all__ = ["parse_json_array"]