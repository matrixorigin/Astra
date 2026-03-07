"""Backward-compat shim — real implementation in core.memory.tabular.sensitivity."""
from core.memory.tabular.sensitivity import *  # noqa: F401,F403
from core.memory.tabular.sensitivity import check_sensitivity, SensitivityResult

__all__ = ["check_sensitivity", "SensitivityResult"]