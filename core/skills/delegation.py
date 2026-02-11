"""Delegation skill for multi-agent collaboration."""

from core.logging_config import get_logger
from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)

logger = get_logger(__name__)


class DelegateTaskInput(SkillInput):
    """Input for delegation skill."""

    agent_id: str
    task: str
    context: str | None = None


class DelegateTaskOutput(SkillOutput):
    """Output for delegation skill."""

    result: str
    agent_id: str
    events_produced: int


class DelegateTaskSkill(Skill):
    """Delegate a task to another agent.

    This skill enables orchestrator agents to delegate work to specialist agents.
    The delegation is logged as an event for auditability.
    """

    name: str = "delegate_task"
    version: str = "1.0.0"
    description: str = "Delegate a task to another agent for execution"
    requirements: SkillRequirement = SkillRequirement(
        repo_types=[RepoType.CODE, RepoType.DOCS], min_access=AccessScope.READ, llm_required=True
    )
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    def __init__(self, agent_registry, chat_loop_factory):
        """Initialize delegation skill.

        Args:
            agent_registry: AgentRegistry instance for looking up agent profiles
            chat_loop_factory: Factory function that creates ChatLoop instances
        """
        self.registry = agent_registry
        self.make_loop = chat_loop_factory

    def validate_input(self, input_data: dict) -> DelegateTaskInput:
        """Validate and parse input data."""
        # Extract session_id and user_id from input or use defaults
        session_id = input_data.get("session_id", "default_session")
        user_id = input_data.get("user_id", "default_user")

        return DelegateTaskInput(
            agent_id=input_data.get("agent_id", ""),
            task=input_data.get("task", ""),
            context=input_data.get("context"),
            session_id=session_id,
            user_id=user_id,
        )

    async def execute(self, input: DelegateTaskInput) -> DelegateTaskOutput:
        """Execute the delegation.

        Args:
            input: DelegateTaskInput with agent_id, task, and optional context

        Returns:
            DelegateTaskOutput with result from delegated agent
        """
        profile = self.registry.get(input.agent_id)

        if not profile:
            return DelegateTaskOutput(
                success=False,
                result=f"Error: Agent '{input.agent_id}' not found",
                agent_id=input.agent_id,
                events_produced=0,
            )

        # Create a new ChatLoop for the delegated agent
        loop = self.make_loop(system_prompt=profile.system_prompt)

        # Execute the task
        result = await loop.run_step(
            user_input=input.task,
            session_id=input.session_id,
            user_id=input.user_id,
            context={"system_prompt": profile.system_prompt},
        )

        return DelegateTaskOutput(
            success=True,
            result=result,
            agent_id=input.agent_id,
            events_produced=0,  # Will be counted by the loop
        )
