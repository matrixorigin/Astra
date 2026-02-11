"""Agent skill execution logic with side-effect isolation."""

from typing import Dict, Any, Optional
from sdk import Database
from core.skills.registry import SkillRegistry
from core.skills.mocking import ToolMockingLayer, MockMode
from core.logging_config import get_logger

logger = get_logger(__name__)

class AgentExecutor:
    """Executes skills safely using the ToolMockingLayer."""

    def __init__(self, db: Database, registry: SkillRegistry, mode: MockMode = MockMode.PRODUCTION):
        self.db = db
        self.registry = registry
        self.mode = mode
        self.mock_layer = ToolMockingLayer(mode, db)

    def execute_skill(
        self, 
        skill_name: str, 
        params: Dict[str, Any], 
        session_id: str,
        parent_event_id: Optional[str] = None
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
                skill=skill,
                params=params,
                session_id=session_id,
                parent_event_id=parent_event_id
            )
            return result
        except Exception as e:
            logger.error(f"Error executing skill {skill_name}: {e}")
            raise
