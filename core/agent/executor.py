"""Agent skill execution logic with side-effect isolation."""

import time
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
        """Execute a single skill safely with automatic metric recording.

        Args:
            skill_name: Name of the skill to execute.
            params: Parameters for the skill.
            session_id: The current session ID.
            parent_event_id: The ID of the parent event (e.g. user message).

        Returns:
            The result of the skill execution.
            
        Side effects:
            Records execution metrics (time, cost) to skill_execution_metrics table.
        """
        logger.info(f"Executing skill: {skill_name} with params: {params}")

        # 1. Get skill from registry
        skill = self.registry.get(skill_name)
        if not skill:
            raise ValueError(f"Skill '{skill_name}' not found in registry.")

        # 2. Execute via Mocking Layer with timing
        start_time = time.time()
        success = True
        cost = 0.0
        result = None
        error_msg = None
        
        try:
            result = self.mock_layer.execute(
                skill=skill, params=params, session_id=session_id, parent_event_id=parent_event_id
            )
            
            # Extract cost if result contains LLM response
            if isinstance(result, dict):
                cost = result.get("cost", 0.0)
            
        except Exception as e:
            logger.error(f"Error executing skill {skill_name}: {e}")
            success = False
            error_msg = str(e)
            raise
        finally:
            execution_time_ms = int((time.time() - start_time) * 1000)
            
            # Record metrics to database
            self._record_execution_metrics(
                skill_name=skill_name,
                session_id=session_id,
                execution_time_ms=execution_time_ms,
                execution_cost=cost,
                success=success,
                error_msg=error_msg,
            )
        
        return result
    
    def _record_execution_metrics(
        self,
        skill_name: str,
        session_id: str,
        execution_time_ms: int,
        execution_cost: float,
        success: bool,
        error_msg: str | None = None,
    ):
        """Record skill execution metrics to database.
        
        This enables multi-dimensional learning from execution performance.
        """
        try:
            from api.models import SkillExecutionMetric
            from uuid_utils import uuid7
            from datetime import datetime, timezone
            
            metric = SkillExecutionMetric(
                metric_id=str(uuid7()),
                session_id=session_id,
                skill_name=skill_name,
                execution_time_ms=execution_time_ms,
                execution_cost=execution_cost,
                success=success,
                error_message=error_msg,
                created_at=datetime.now(timezone.utc),
            )
            self.db.add(metric)
            self.db.commit()
            
            logger.debug(
                f"Recorded metrics for {skill_name}: "
                f"time={execution_time_ms}ms, cost=${execution_cost:.4f}, success={success}"
            )
        except Exception as e:
            logger.error(f"Failed to record execution metrics: {e}")
            # Don't fail the execution if metrics recording fails
            self.db.rollback()
    
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
