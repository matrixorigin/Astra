"""Backward-compat shim — real implementation in core.memory.tabular.typed_observer."""
from core.memory.tabular.typed_observer import *  # noqa: F401,F403
from core.memory.tabular.typed_observer import TypedObserver, _parse_json_array  # noqa: F401

__all__ = ["TypedObserver"]
