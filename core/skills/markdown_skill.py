"""MarkdownSkill — wraps a SKILL.md as a Skill for the tool router."""

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
from core.skills.skill_md import SkillMd


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
