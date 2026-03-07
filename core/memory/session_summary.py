"""Backward-compat shim — real implementation in core.memory.tabular.session_summary."""
from core.memory.tabular.session_summary import *  # noqa: F401,F403
from core.memory.tabular.session_summary import (  # noqa: F401
    SessionSummarizer, _SESSION_SUMMARY_TAG, _INCREMENTAL_TAG,
)

__all__ = ["SessionSummarizer"]
