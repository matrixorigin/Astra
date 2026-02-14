"""Agent skill execution logic with side-effect isolation."""

from typing import Any

from core.logging_config import get_logger
from core.skills.mocking import MockMode, ToolMockingLayer
from core.skills.registry import SkillRegistry
from sqlalchemy.orm import Session
from api.database import get_db_session

logger = get_logger(__name__)


class AgentExecutor:
    """Executes skills safely using the ToolMockingLayer."""

    def __init__(self, db: Session, registry: SkillRegistry, mode: MockMode = MockMode.PRODUCTION):
        self.db = db
        self.registry = registry
        self.mode = mode
        self.mock_layer = ToolMockingLayer(mode, db)

    def execute_skill(
        self,
        skill_name: str,
        params: dict[str, Any],
        session_id: str,
        parent_event_id: str | None = None,
    ) -> Any:
        """Execute a single skill safely.

        Args:
            skill_name: Name of the skill to execute.
            params: Parameters for the skill.
            session_id: The current session ID.
            parent_event_id: The ID of the parent event (e.g. user message).

        Returns:
            The result of the skill execution.
        """
        logger.info(f"Executing skill: {skill_name} with params: {params}")

        # 1. Get skill from registry
        skill = self.registry.get(skill_name)
        if not skill:
            raise ValueError(f"Skill '{skill_name}' not found in registry.")

        # 2. Execute via Mocking Layer (handles side-effect isolation)
        try:
            result = self.mock_layer.execute(
                skill=skill, params=params, session_id=session_id, parent_event_id=parent_event_id
            )
            return result
        except Exception as e:
            logger.error(f"Error executing skill {skill_name}: {e}")
            raise
    
    async def execute_skill_stream(
        self,
        skill_name: str,
        params: dict[str, Any],
        session_id: str,
        parent_event_id: str | None = None,
    ):
        """Execute a skill with streaming output.
        
        Args:
            skill_name: Name of the skill to execute.
            params: Parameters for the skill.
            session_id: The current session ID.
            parent_event_id: The ID of the parent event.
            
        Yields:
            StreamEvent: Stream events from skill execution
        """
        logger.info(f"Executing skill (streaming): {skill_name} with params: {params}")

        # 1. Get skill from registry
        skill = self.registry.get(skill_name)
        if not skill:
            raise ValueError(f"Skill '{skill_name}' not found in registry.")

        # 2. Check if skill supports streaming
        if not hasattr(skill, 'execute_stream'):
            # Fall back to non-streaming execution
            result = self.mock_layer.execute(
                skill=skill, params=params, session_id=session_id, parent_event_id=parent_event_id
            )
            # Yield result as single event
            from core.events.models import StreamEvent, StreamEventType
            yield StreamEvent(
                event_type=StreamEventType.TOOL_RESULT,
                data={"result": str(result)},
            )
            return
        
        # 3. Execute with streaming
        try:
            async for event in skill.execute_stream(skill.validate_input(params)):
                yield event
        except Exception as e:
            logger.error(f"Error executing skill {skill_name}: {e}")
            from core.events.models import StreamEvent, StreamEventType
            yield StreamEvent(
                event_type=StreamEventType.RUN_ERROR,
                data={"error": str(e)},
            )
