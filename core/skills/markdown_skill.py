"""MarkdownSkill — wraps a SKILL.md as a Skill for the tool router."""

from typing import Any

from cli.tools.base import SideEffect
from core.skills.base import (
    RuntimeRequirement,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)
from core.skills.skill_md import SkillMd

# SideEffectCategory → SideEffect mapping for permission system compatibility.
# MarkdownSkills only use READ; if new categories are added, extend this map.
_CATEGORY_TO_SIDE_EFFECT: dict[SideEffectCategory, SideEffect] = {
    SideEffectCategory.READ: SideEffect.READ,
    SideEffectCategory.WRITE: SideEffect.WRITE,
    SideEffectCategory.EXECUTE: SideEffect.EXECUTE,
}


class MarkdownSkillInput(SkillInput):
    """Input for markdown-based skills — user query is the only input."""

    query: str = ""


class MarkdownSkillOutput(SkillOutput):
    """Output is the skill's markdown instructions for the LLM."""

    instructions: str = ""


class MarkdownSkill(Skill[MarkdownSkillInput, MarkdownSkillOutput]):
    """A skill defined by a SKILL.md file.

    When executed, returns the markdown body as instructions for the LLM.
    The LLM interprets these instructions to fulfill the user's request.
    """

    def __init__(self, spec: SkillMd):
        self.name = spec.name
        self.version = spec.version
        self.description = spec.description
        self._body = spec.body
        self._spec = spec
        self.requirements = SkillRequirement(
            runtime=[RuntimeRequirement.NONE],
            llm_required=spec.llm_required,
        )
        self.side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    @property
    def side_effect(self) -> SideEffect:
        """Permission-system compatible side effect, derived from side_effect_profile.

        The edge_chat_loop permission system requires SideEffect (cli enum),
        not SideEffectCategory (core enum).  This property bridges the two
        so MarkdownSkills work in the ToolRouter alongside EdgeTools.
        """
        return _CATEGORY_TO_SIDE_EFFECT.get(
            self.side_effect_profile.category,
            SideEffect.READ,
        )

    async def execute(self, input: MarkdownSkillInput) -> MarkdownSkillOutput:
        return MarkdownSkillOutput(
            success=True,
            result=self._body,
            instructions=self._body,
        )

    def to_openai_schema(self) -> dict[str, Any]:
        params: dict[str, Any] = {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The user's request or question for this skill",
                },
            },
        }
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": params,
            },
        }
