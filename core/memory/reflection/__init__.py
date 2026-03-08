"""Shared reflection engine — backend-agnostic pattern synthesis.

See docs/design/memory/backend-coexistence.md
"""

from core.memory.reflection.engine import ReflectionEngine
from core.memory.reflection.importance import score_candidate
from core.memory.reflection.opinion import OpinionEvolver

__all__ = ["ReflectionEngine", "score_candidate", "OpinionEvolver"]
