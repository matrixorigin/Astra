"""Agent skill selection logic."""

from typing import Any

from core.logging_config import get_logger
from core.skills.modern_selector import ModernSkillSelector
from sdk import Database

logger = get_logger(__name__)


class AgentSkillSelector:
    """Selects skills for the agent to execute."""

    def __init__(self, db: Database, llm_client):
        self.selector = ModernSkillSelector(db, llm_client)

    def select_skills(
        self, query: str, context: dict[str, Any] | None = None, max_candidates: int = 5
    ) -> list[dict[str, Any]]:
        """Select skills based on query and context.

        Args:
            query: The user's query or last message.
            context: Context dictionary (e.g. conversation history).
            max_candidates: Maximum number of skills to consider.

        Returns:
            List of tool calls (dict with 'function' and 'arguments').
        """
        return self.selector.select_and_execute(query, context, max_candidates)
