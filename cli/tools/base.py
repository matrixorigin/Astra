"""Base class for edge tools — thin adapter over core Skill framework.

EdgeTool is a Skill subclass that preserves the simple ``execute(**kwargs) -> str``
interface used by file_ops, shell, git, search, and introspection tools.
All EdgeTools are Skills; the ToolRouter and SkillExecutor treat them uniformly.
"""

from abc import abstractmethod
from enum import Enum
from typing import Any

from core.skills.base import (
    RuntimeRequirement,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)


class SideEffect(str, Enum):
    """Side effect classification for permission checking.

    Maps 1:1 to SideEffectCategory but kept as the public API for edge tools
    and the permission system.
    """

    READ = "read"
    WRITE = "write"
    EXECUTE = "execute"


# Mapping from SideEffect → SideEffectCategory
_SIDE_EFFECT_MAP: dict[SideEffect, SideEffectCategory] = {
    SideEffect.READ: SideEffectCategory.READ,
    SideEffect.WRITE: SideEffectCategory.WRITE,
    SideEffect.EXECUTE: SideEffectCategory.EXECUTE,
}

# Reverse mapping: SideEffectCategory → SideEffect
_CATEGORY_TO_SIDE_EFFECT: dict[SideEffectCategory, SideEffect] = {
    v: k for k, v in _SIDE_EFFECT_MAP.items()
}


def resolve_side_effect(tool: Any) -> SideEffect:
    """Get the SideEffect for any Skill/EdgeTool.

    EdgeTools have ``side_effect`` directly.  Typed Skills loaded from
    skill.py only carry ``side_effect_profile`` (core enum).  This
    function bridges both so the permission system works uniformly.
    """
    se = getattr(tool, "side_effect", None)
    if isinstance(se, SideEffect):
        return se
    cat = getattr(getattr(tool, "side_effect_profile", None), "category", None)
    return _CATEGORY_TO_SIDE_EFFECT.get(cat, SideEffect.READ)


class EdgeTool(Skill[SkillInput, SkillOutput]):
    """Skill adapter for tools that run on the user's machine.

    Subclasses define ``name``, ``description``, ``parameters`` (JSON Schema dict),
    ``side_effect``, and ``async execute(**kwargs) -> str``.  The Skill framework
    fields (``requirements``, ``side_effect_profile``, ``to_openai_schema``) are
    derived automatically.
    """

    # Subclasses set these as class attributes or properties
    name: str
    description: str
    parameters: dict[str, Any]
    side_effect: SideEffect

    # Default: edge tools need local filesystem
    requirements: SkillRequirement = SkillRequirement(
        runtime=[RuntimeRequirement.FILESYSTEM],
        llm_required=False,
    )

    @property
    def side_effect_profile(self) -> SideEffectProfile:  # type: ignore[override]
        return SideEffectProfile(category=_SIDE_EFFECT_MAP[self.side_effect])

    @abstractmethod
    async def execute(self, **kwargs: Any) -> str:  # type: ignore[override]
        """Execute the tool and return result as string."""

    def to_openai_schema(self) -> dict[str, Any]:
        """Return OpenAI function calling tool schema."""
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            },
        }
