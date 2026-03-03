"""Introspection skill — answers questions about the agent's own state.

Handles queries like "how big is my context?", "how many turns?",
"what model are you using?", "what can you do?" without requiring
a full LLM reasoning cycle.  The skill pipeline selects this via
trigger keywords / semantic match, then the executor returns
pre-computed runtime stats.
"""

from __future__ import annotations

from typing import Any

from pydantic import Field

from core.logging_config import get_logger
from core.skills.base import (
    AccessScope,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)

logger = get_logger(__name__)


class IntrospectionInput(SkillInput):
    """Input for introspection queries."""

    dimension: str = Field(
        default="all",
        description="Which dimension to query: context, session, capability, or all",
    )
    runtime_state: dict[str, Any] = Field(
        default_factory=dict,
        description="Runtime state injected by the executor at call time",
    )


class IntrospectionOutput(SkillOutput):
    """Output with agent runtime stats."""

    context_tokens: int = 0
    max_tokens: int = 0
    usage_percent: float = 0.0
    turn_count: int = 0
    session_id: str | None = None
    agent_id: str | None = None
    model: str | None = None
    skills_loaded: int = 0


class IntrospectionSkill(Skill[IntrospectionInput, IntrospectionOutput]):
    """Answer questions about the agent's own runtime state.

    Zero LLM cost — returns pre-computed stats from the session context.
    """

    name = "introspection"
    version = "1.0.0"
    description = (
        "Answer questions about the agent itself: context window size, "
        "token usage, session state, turn count, loaded skills, model info, "
        "and capabilities. Use when the user asks about context size, "
        "how many turns, what model, or agent status."
    )
    requirements = SkillRequirement(
        repo_types=[],
        min_access=AccessScope.READ,
        llm_required=False,
        timeout_seconds=5,
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ,
        external_apis=[],
    )

    def __init__(self, db_factory=None) -> None:
        self._db_factory = db_factory

    async def execute(self, input: IntrospectionInput) -> IntrospectionOutput:
        """Return runtime stats from session context.

        The actual values are injected by the executor at runtime via
        input.runtime_state — this skill just formats them.
        """
        meta = input.runtime_state

        context_tokens = meta.get("context_tokens", 0)
        max_tokens = meta.get("max_tokens", 128000)
        usage_pct = (context_tokens / max_tokens * 100) if max_tokens > 0 else 0.0

        return IntrospectionOutput(
            success=True,
            result=f"Context: {context_tokens}/{max_tokens} tokens ({usage_pct:.1f}%)",
            context_tokens=context_tokens,
            max_tokens=max_tokens,
            usage_percent=round(usage_pct, 1),
            turn_count=meta.get("turn_count", 0),
            session_id=meta.get("session_id"),
            agent_id=meta.get("agent_id"),
            model=meta.get("model"),
            skills_loaded=meta.get("skills_loaded", 0),
        )
