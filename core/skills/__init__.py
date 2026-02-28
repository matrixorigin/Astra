"""Skills package for mo-agent-engine."""

from .base import (
    AccessScope,
    RepoType,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)
from .catalog import NameConflictError, SkillCatalog

# Backward-compat alias lives in registry.py (the canonical shim).
# Re-export here so ``from core.skills import SkillRegistry`` works.
from .registry import SkillRegistry

__all__ = [
    "AccessScope",
    "NameConflictError",
    "RepoType",
    "Skill",
    "SkillCatalog",
    "SkillInput",
    "SkillOutput",
    "SkillRegistry",
    "SkillRequirement",
]
