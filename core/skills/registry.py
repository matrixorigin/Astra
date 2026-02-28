"""Backward-compatibility shim — SkillRegistry is now SkillCatalog.

All code should migrate to ``from core.skills.catalog import SkillCatalog``.
This module re-exports SkillCatalog as SkillRegistry for existing imports.
"""

from core.skills.catalog import SkillCatalog as SkillRegistry  # noqa: F401

__all__ = ["SkillRegistry"]
