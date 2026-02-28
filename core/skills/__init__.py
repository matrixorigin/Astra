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

# Backward-compat alias — existing code imports SkillRegistry
SkillRegistry = SkillCatalog

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
