"""Agent skill execution logic with side-effect isolation."""

import time
from typing import Any, TYPE_CHECKING

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger
from core.skills.mocking import MockMode, ToolMockingLayer
from core.skills.registry import SkillRegistry

if TYPE_CHECKING:
    from core.skills.pipeline import SkillPipeline
    from core.skills.skill_manager import SkillManager

logger = get_logger(__name__)


class AgentExecutor(DbConsumer):
    """Executes skills safely using the ToolMockingLayer."""

    def __init__(
        self,
        db_factory: DbFactory,
        registry: SkillRegistry,
        mode: MockMode = MockMode.PRODUCTION,
        pipeline: "SkillPipeline | None" = None,
        skill_manager: "SkillManager | None" = None,
    ):
        super().__init__(db_factory)
        self.registry = registry
        self.mode = mode
        self.mock_layer = ToolMockingLayer(mode, db_factory)
        self._pipeline = pipeline
        self._skill_manager = skill_manager

        from core.agent.execution_backend import BackendRouter
        self._backend_router = BackendRouter()

    def _enforce_runtime_checks(self, skill_name: str, params: dict[str, Any]) -> None:
        """Enforce installation + permission + dependency checks for marketplace skills."""
        if not self._skill_manager:
            return
        user_id = params.get("user_id", "system")
        self._skill_manager.require_executable(user_id, skill_name)

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

        # 1.5. Auto-inject framework fields (user_id, session_id)
        params.setdefault("session_id", session_id)
        if "user_id" not in params:
            params["user_id"] = getattr(self, "_current_user_id", "system")

        # 1.5b. Auto-inject runtime_state for introspection skill.
        # The introspection skill needs live context stats that only the
        # executor knows at call time.  Other skills ignore this field.
        if skill_name == "introspection":
            params.setdefault("runtime_state", self._build_runtime_state(session_id))

        # 1.6. Enforce skill installation for marketplace skills
        self._enforce_runtime_checks(skill_name, params)

        # 2. Check if skill needs heavyweight backend
        exec_req = self._get_execution_requirements(skill)
        if not self._backend_router.is_lightweight(exec_req):
            return self._execute_heavyweight_sync(skill_name, params, exec_req, session_id)

        # 3. Execute via Mocking Layer with timing (in-process, zero overhead)
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
    
    def execute_skill_with_feedback(
        self,
        skill_name: str,
        params: dict[str, Any],
        session_id: str,
        parent_event_id: str | None = None,
        selection_event_id: str | None = None,
        extra_feedback_data: dict[str, Any] | None = None,
    ) -> Any:
        """Execute a skill and automatically record execution time feedback.
        
        This method encapsulates the execution + feedback recording pattern,
        preventing responsibility leakage to the caller (ChatLoop).
        
        Args:
            skill_name: Name of the skill to execute.
            params: Parameters for the skill.
            session_id: The current session ID.
            parent_event_id: The ID of the parent event.
            selection_event_id: Event ID from skill selection (for feedback).
            extra_feedback_data: Additional data to include in feedback (e.g., planning_step).
            
        Returns:
            The result of the skill execution.
        """
        _t0 = time.monotonic()
        _success = True
        _cost = 0.0
        _result = None
        try:
            _result = self.execute_skill(
                skill_name=skill_name,
                params=params,
                session_id=session_id,
                parent_event_id=parent_event_id,
            )
            if isinstance(_result, dict):
                _cost = _result.get("cost", 0.0)
            return _result
        except Exception:
            _success = False
            raise
        finally:
            _elapsed_ms = (time.monotonic() - _t0) * 1000

            # Write back execution metrics to skill_selection_events
            # so learn_from_failures() can read them
            if selection_event_id:
                self._backfill_selection_event(
                    selection_event_id, int(_elapsed_ms), _cost, _success,
                )

            # Buffer feedback signal (existing path)
            if self._pipeline and selection_event_id:
                from core.skills.learning_signals import SignalType, SignalThresholds
                
                feedback_data: dict[str, Any] = {
                    "ms": _elapsed_ms,
                    "skill": skill_name,
                    "actual_usd": _cost,
                }
                if extra_feedback_data:
                    feedback_data.update(extra_feedback_data)
                
                self._pipeline.record_feedback(
                    selection_event_id,
                    SignalType.EXECUTION_TIME,
                    feedback_data,
                )

                # Emit HIGH_COST signal when cost exceeds threshold
                thresholds = SignalThresholds()
                if _cost > thresholds.high_cost_usd:
                    self._pipeline.record_feedback(
                        selection_event_id,
                        SignalType.HIGH_COST,
                        {
                            "skill": skill_name,
                            "actual_usd": _cost,
                            "actual_tokens": extra_feedback_data.get("actual_tokens", 0) if extra_feedback_data else 0,
                            "threshold_usd": thresholds.high_cost_usd,
                        },
                    )
    
    def _backfill_selection_event(
        self, event_id: str, time_ms: int, cost: float, success: bool,
    ) -> None:
        """Update skill_selection_events with post-execution metrics.

        Uses an independent session (via self._db()) intentionally: backfill
        is best-effort and must not interfere with the caller's transaction.
        If it fails, the execution result is still returned to the user.
        """
        try:
            from api.models.skill import SkillSelectionEvent
            with self._db() as db:
                evt = db.query(SkillSelectionEvent).filter(
                    SkillSelectionEvent.event_id == event_id,
                ).first()
                if evt:
                    evt.execution_time_ms = time_ms
                    evt.execution_cost = cost
                    evt.execution_success = 1 if success else 0
                    db.commit()
                else:
                    logger.debug("backfill: event_id=%s not found, skipping", event_id)
        except Exception as e:
            logger.debug("backfill selection event failed: %s", e)

    def _build_runtime_state(self, session_id: str) -> dict[str, Any]:
        """Collect live runtime stats for the introspection skill."""
        state: dict[str, Any] = {"session_id": session_id}
        try:
            from api.models.agent import Event, Session as SessionModel
            from api.models.skill import SkillRegistry as SkillModel
            from sqlalchemy import func
            with self._db() as db:
                sess = db.query(SessionModel.agent_id, SessionModel.event_count).filter(
                    SessionModel.session_id == session_id
                ).first()
                if sess:
                    state["agent_id"] = sess.agent_id
                    state["turn_count"] = sess.event_count or 0
                state["skills_loaded"] = db.query(func.count()).select_from(
                    SkillModel
                ).filter(SkillModel.is_active == 1).scalar() or 0
        except Exception as e:
            logger.debug("Failed to build runtime_state: %s", e)
        return state

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
            with self._db() as db:
                db.add(metric)
                db.commit()
            
            logger.debug(
                f"Recorded metrics for {skill_name}: "
                f"time={execution_time_ms}ms, cost=${execution_cost:.4f}, success={success}"
            )
        except Exception as e:
            logger.error(f"Failed to record execution metrics: {e}")
    
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

        # 1.5. Auto-inject framework fields
        params.setdefault("session_id", session_id)
        if "user_id" not in params:
            params["user_id"] = getattr(self, "_current_user_id", "system")

        # 1.6. Enforce runtime checks (same as non-streaming path)
        self._enforce_runtime_checks(skill_name, params)

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

    @staticmethod
    def _get_execution_requirements(skill) -> "ExecutionRequirements":
        """Extract ExecutionRequirements from skill's SkillRequirement."""
        from core.agent.execution_backend import ExecutionRequirements
        from core.skills.base import SkillRequirement
        req = getattr(skill, "requirements", None)
        if not isinstance(req, SkillRequirement):
            return ExecutionRequirements()
        return ExecutionRequirements(
            gpu_required=req.gpu_required,
            conda_env=req.conda_env,
            timeout_seconds=req.timeout_seconds,
            min_memory_gb=req.min_memory_gb,
        )

    def _execute_heavyweight_sync(
        self, skill_name: str, params: dict, req: "ExecutionRequirements", session_id: str,
    ) -> Any:
        """Route heavyweight skill to subprocess (blocking, for sync callers)."""
        import json as _json
        import os
        import subprocess
        import sys

        cmd = [sys.executable, "-m", "core.skills.runner",
               "--skill", skill_name, "--inputs", _json.dumps(params)]
        if req.conda_env:
            cmd = ["conda", "run", "-n", req.conda_env, "--no-capture-output"] + cmd

        start_time = time.time()
        try:
            env = {**os.environ, **req.env_vars} if req.env_vars else None
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=req.timeout_seconds, env=env)
            elapsed_ms = int((time.time() - start_time) * 1000)

            if proc.returncode != 0:
                err_text = (proc.stderr or "")[-2000:]
                self._record_execution_metrics(
                    skill_name=skill_name, session_id=session_id,
                    execution_time_ms=elapsed_ms, execution_cost=0.0,
                    success=False, error_msg=err_text,
                )
                raise RuntimeError(f"Skill {skill_name} failed: {err_text[-500:]}")

            try:
                result = _json.loads(proc.stdout) if proc.stdout and proc.stdout.strip() else {}
            except (ValueError, _json.JSONDecodeError):
                result = {"output": (proc.stdout or "")[-2000:]}

            self._record_execution_metrics(
                skill_name=skill_name, session_id=session_id,
                execution_time_ms=elapsed_ms, execution_cost=0.0, success=True,
            )
            return result
        except subprocess.TimeoutExpired:
            self._record_execution_metrics(
                skill_name=skill_name, session_id=session_id,
                execution_time_ms=req.timeout_seconds * 1000, execution_cost=0.0,
                success=False, error_msg="Timeout",
            )
            raise RuntimeError(f"Skill {skill_name} timed out after {req.timeout_seconds}s")
