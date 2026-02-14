"""Agent skill selection logic with self-improving capabilities."""

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Any

from core.logging_config import get_logger
from core.skills.auditable_selector import AuditableSkillSelector
from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.regression_gate import SkillSelectionRegressionGate
from sqlalchemy.orm import Session

logger = get_logger(__name__)


@dataclass
class SkillCandidate:
    """Skill candidate for learning application."""
    name: str
    version: str = "1.0.0"
    confidence: float = 1.0


class AgentSkillSelector:
    """Selects skills for the agent with self-improving capabilities.
    
    Integrates:
    - AuditableSkillSelector: Full audit trail of every selection
    - SelfImprovingSelector: Learns from historical failures
    - RegressionGate: Validates learning before deployment
    """

    def __init__(
        self,
        db: Session,
        llm_client,
        auditable: bool = True,
        session_id: str | None = None,
        enable_learning: bool = True,
        learning_cooldown_hours: int = 1,  # Cooldown period between learning cycles
        learning_weights: "SignalWeights | None" = None,  # Multi-dimensional weights
    ):
        """Initialize skill selector with self-improvement.
        
        Args:
            db: Session instance
            llm_client: LLM client
            auditable: Use auditable selector (default True)
            session_id: Session ID for auditable selections
            enable_learning: Enable self-improving selector (default True)
            learning_cooldown_hours: Hours between learning cycles (default 1)
            learning_weights: Custom weights for multi-dimensional scoring
        """
        self.db = db
        self.llm_client = llm_client
        self.session_id = session_id
        self.auditable = auditable
        self.enable_learning = enable_learning
        self.learning_cooldown_hours = learning_cooldown_hours
        
        # Core selector with audit trail
        self.auditable_selector = AuditableSkillSelector(db, llm_client)
        
        # Self-improving layer
        if enable_learning:
            self.improving_selector = SelfImprovingSelector(db, llm_client, weights=learning_weights)
            self.regression_gate = SkillSelectionRegressionGate(llm_client, db)
            logger.info("Self-improving selector enabled - learning from failures")
        
        self._last_selection_event_id = None
        self._last_learning_time: datetime | None = None

    def select_skills(
        self, query: str, context: dict[str, Any] | None = None, max_candidates: int = 5
    ) -> list[dict[str, Any]]:
        """Select skills with self-improvement.

        Args:
            query: The user's query or last message.
            context: Context dictionary (e.g. conversation history).
            max_candidates: Maximum number of skills to consider.

        Returns:
            List of tool calls (dict with 'function' and 'arguments').
        """
        if not self.auditable or not self.session_id:
            # Fallback to basic selection
            return self.auditable_selector.select_and_execute(query, context, max_candidates)
        
        # Step 1: Get candidates from auditable selector
        event = self.auditable_selector.select_with_validation(
            query=query,
            session_id=self.session_id,
            validate_in_sandbox=False
        )
        
        # Step 2: Apply learned corrections if enabled
        if self.enable_learning and event.selected_skills:
            candidates = [
                SkillCandidate(name=name)
                for name in event.selected_skills
            ]
            corrected = self.improving_selector.apply_learnings(query, candidates)
            event.selected_skills = [c.name for c in corrected]
            
            if len(corrected) != len(candidates):
                logger.info(f"Applied learning correction: {len(candidates)} → {len(corrected)} skills")
        
        # Step 3: Convert to tool calls format
        tool_calls = []
        for skill_name in event.selected_skills:
            tool_calls.append({
                "function": {
                    "name": skill_name,
                    "arguments": "{}"
                }
            })
        
        self._last_selection_event_id = event.event_id
        return tool_calls
    
    def get_tools_schema(self, query: str, max_candidates: int = 5) -> list[dict]:
        """Get tools schema for LLM function calling.
        
        Args:
            query: User query for context-aware tool selection
            max_candidates: Maximum number of tools to return
            
        Returns:
            List of tool schemas in OpenAI format
        """
        # Delegate to modern selector for schema generation
        return self.auditable_selector.modern_selector.get_tools_schema(query, max_candidates)
    
    def learn_from_failures(
        self, days: int = 7, force: bool = False, signal_types: list["SignalType"] | None = None
    ) -> dict[str, Any]:
        """Trigger learning from recent failures with regression gating.
        
        This is the breakthrough: automatic learning with safety validation.
        
        Args:
            days: Look back N days for failures
            force: Force learning even if in cooldown period
            signal_types: Types of signals to learn from (default: all)
            
        Returns:
            Learning results with gate validation
        """
        if not self.enable_learning:
            return {"error": "Learning disabled"}
        
        # Check cooldown period
        if not force and self._last_learning_time:
            cooldown_end = self._last_learning_time + timedelta(hours=self.learning_cooldown_hours)
            if datetime.now(timezone.utc) < cooldown_end:
                remaining = (cooldown_end - datetime.now(timezone.utc)).total_seconds() / 3600
                logger.info(f"Learning in cooldown period ({remaining:.1f}h remaining)")
                return {
                    "error": "cooldown",
                    "message": f"Learning cooldown active, {remaining:.1f}h remaining",
                    "cooldown_hours": self.learning_cooldown_hours,
                }
        
        logger.info(f"Starting learning cycle - analyzing last {days} days")
        
        try:
            # Step 1: Learn from failures (multi-dimensional)
            learn_result = self.improving_selector.learn_from_failures(
                days=days, signal_types=signal_types
            )
            
            if learn_result["learned"] == 0:
                logger.info("No new learnings")
                self._last_learning_time = datetime.now(timezone.utc)
                return learn_result
            
            logger.info(f"Learned {learn_result['learned']} corrections")
            if "signals_by_type" in learn_result:
                logger.info(f"Signals by type: {learn_result['signals_by_type']}")
            
            # Step 2: Validate through regression gate
            golden_queries = self.regression_gate.get_golden_queries(limit=20)
            
            if not golden_queries:
                logger.warning("No golden queries for regression testing - deploying without gate")
                self._last_learning_time = datetime.now(timezone.utc)
                return {**learn_result, "gate_verdict": "skipped", "reason": "no_golden_queries"}
            
            # Create old/new selector for comparison
            old_selector = AuditableSkillSelector(self.db, self.llm_client)
            new_selector = AuditableSkillSelector(self.db, self.llm_client)
            
            gate_result = self.regression_gate.validate_selector_change(
                new_selector=new_selector,
                old_selector=old_selector,
                test_queries=golden_queries,
                min_improvement_pct=0.0,
            )
            
            logger.info(f"Regression gate: {gate_result['verdict']} (improvement: {gate_result['improvement_pct']:.1f}%)")
            
            # Step 3: Record to learning log
            self._record_learning_cycle(learn_result, gate_result)
            
            # Update last learning time
            self._last_learning_time = datetime.now(timezone.utc)
            
            return {
                **learn_result,
                "gate_verdict": gate_result["verdict"],
                "improvement_pct": gate_result["improvement_pct"],
                "test_count": gate_result["test_count"],
            }
            
        except Exception as e:
            logger.error(f"Learning cycle failed: {e}")
            return {
                "error": "learning_failed",
                "message": str(e),
                "learned": 0,
            }
    
    def _record_learning_cycle(self, learn_result: dict, gate_result: dict):
        """Record learning cycle to database for audit trail."""
        from api.models import SelectorGateResult
        from uuid_utils import uuid7
        from datetime import datetime, timezone
        
        try:
            record = SelectorGateResult(
                gate_id=str(uuid7()),
                selector_version="self_improving_v1",
                test_queries=[],  # Not storing queries for now
                verdict=gate_result["verdict"].upper(),
                new_avg_score=gate_result["new_avg_score"],
                old_avg_score=gate_result["old_avg_score"],
                improvement_pct=gate_result["improvement_pct"],
                test_count=gate_result["test_count"],
                learnings_applied=learn_result["learned"],
                created_at=datetime.now(timezone.utc),
            )
            self.db.add(record)
            self.db.commit()
            logger.info(f"Recorded learning cycle: gate_id={record.gate_id}")
        except Exception as e:
            logger.error(f"Failed to record learning cycle: {e}")
            self.db.rollback()
    
    def get_learning_stats(self) -> dict[str, Any]:
        """Get statistics about learning progress."""
        if not self.enable_learning:
            return {"error": "Learning disabled"}
        
        selector_stats = self.improving_selector.get_learning_stats()
        gate_stats = self.regression_gate.get_gate_stats()
        
        return {
            "learnings": selector_stats,
            "regression_gates": gate_stats,
        }
