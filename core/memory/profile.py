"""Backward-compat shim — real implementation in core.memory.tabular.profile."""
from core.memory.tabular.profile import *  # noqa: F401,F403
from core.memory.tabular.profile import ProfileManager, _DEFAULT_PROFILE  # noqa: F401

__all__ = ["ProfileManager"]
