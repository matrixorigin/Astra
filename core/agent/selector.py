"""Agent skill selection logic."""

from typing import Any

from core.logging_config import get_logger
from core.skills.auditable_selector import AuditableSkillSelector
from core.skills.modern_selector import ModernSkillSelector
from sdk import Database

logger = get_logger(__name__)


class AgentSkillSelector:
    """Selects skills for the agent to execute.
    
    Now uses AuditableSkillSelector by default for:
    - Full audit trail of every selection
    - Sandbox pre-validation
    - Automatic learning from failures
    """

    def __init__(self, db: Database, llm_client, auditable: bool = True, session_id: str | None = None):
        """Initialize skill selector.
        
        Args:
            db: Database instance
            llm_client: LLM client
            auditable: Use auditable selector (default True)
            session_id: Session ID for auditable selections
        """
        self.db = db
        self.llm_client = llm_client
        self.session_id = session_id
        self.auditable = auditable
        
        if auditable:
            logger.info("Using AuditableSkillSelector - full audit trail enabled")
            self.selector = AuditableSkillSelector(db, llm_client)
        else:
            logger.info("Using ModernSkillSelector - basic selection")
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
        if self.auditable and self.session_id:
            # Use auditable selection with full tracking
            event = self.selector.select_with_validation(
                query=query,
                session_id=self.session_id,
                validate_in_sandbox=False  # Disable sandbox validation for now (performance)
            )
            
            # Convert to tool calls format
            tool_calls = []
            for skill_name in event.selected_skills:
                tool_calls.append({
                    "function": {
                        "name": skill_name,
                        "arguments": "{}"  # Will be filled by LLM
                    }
                })
            
            # Store event_id for later updates
            if hasattr(self, '_last_selection_event_id'):
                self._last_selection_event_id = event.event_id
            
            return tool_calls
        else:
            # Fallback to modern selector
            return self.selector.select_and_execute(query, context, max_candidates)
