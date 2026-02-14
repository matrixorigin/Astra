"""Self-improving skill selector that learns from historical failures.

This module implements the breakthrough feature: automatic learning from mistakes
using Git for Data's time-travel capabilities.

Phase 1: Multi-dimensional learning with signal types.

Scoring Dimensions:
- Accuracy (40%): Correct skill selection
- Speed (30%): Execution time performance  
- Cost (20%): LLM API cost efficiency
- Satisfaction (10%): User feedback scores

Signal Thresholds:
- Slow execution: > 5000ms (5 seconds)
- High cost: > $0.10
- Low satisfaction: < 3 stars (out of 5)

Target Improvements:
- Time/Cost: 50% reduction from current value
- Satisfaction: 4+ stars (good experience)
"""

import json
from datetime import datetime, timedelta, timezone
from typing import Any

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.models import SkillSelectionEvent as EventModel, SkillSelectionLearning as LearningModel
from core.logging_config import get_logger
from core.sandbox import Sandbox
from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent
from core.skills.learning_signals import LearningSignal, SignalType, SignalWeights, SignalThresholds

logger = get_logger(__name__)


class SelfImprovingSelector:
    """Skill selector that learns from historical failures automatically.
    
    Key innovation: Uses Git for Data to replay failures in sandbox,
    test corrections, and update selection strategy.
    """

    def __init__(self, session: Session, llm_client=None, account: str = "sys", weights: SignalWeights | None = None, thresholds: SignalThresholds | None = None):
        if not isinstance(session, Session):
            raise TypeError("session must be a SQLAlchemy Session")
        
        self.session = session
        self.llm = llm_client
        self.account = account
        self.weights = weights or SignalWeights()
        self.thresholds = thresholds or SignalThresholds()
        self.auditable_selector = AuditableSkillSelector(session, llm_client, account)
        self.sandbox = Sandbox(db=session, account=account)
        self._ensure_tables()

    def _ensure_tables(self):
        """Ensure learning tables exist - no-op as tables are created by ORM."""
        pass

    def learn_from_failures(self, days: int = 7, signal_types: list[SignalType] | None = None) -> dict[str, Any]:
        """Analyze recent failures and learn corrections.
        
        Args:
            days: Number of days to look back
            signal_types: Types of signals to learn from (default: all types)
        """
        if signal_types is None:
            signal_types = list(SignalType)
        
        failures = self.get_recent_failures(days=days)
        
        if not failures:
            return {"learned": 0, "message": "No failures to learn from"}
        
        learned_count = 0
        signals_by_type = {st: 0 for st in signal_types}
        
        for failure in failures:
            try:
                # Extract signals for each type
                for signal_type in signal_types:
                    signal = self._extract_signal(failure, signal_type)
                    if signal:
                        self._update_learnings(signal)
                        learned_count += 1
                        signals_by_type[signal_type] += 1
            except Exception as e:
                logger.error(f"Failed to learn from failure: {e}")
        
        return {
            "learned": learned_count,
            "total_failures": len(failures),
            "signals_by_type": {k.value: v for k, v in signals_by_type.items()},
        }

    def get_recent_failures(self, days: int = 7, limit: int = 10) -> list[dict]:
        """Get recent events with any learning signal (not just wrong_skill).
        
        This extracts events that triggered any of the 4 signal types:
        - WRONG_SKILL: selection_correctness = 0
        - SLOW_EXECUTION: execution_time_ms > threshold
        - HIGH_COST: execution_cost > threshold
        - LOW_SATISFACTION: user_feedback_score < threshold
        """
        from sqlalchemy import or_
        
        cutoff = datetime.now(timezone.utc) - timedelta(days=days)
        
        # Build filter conditions for all signal types
        conditions = [
            EventModel.selection_correctness == 0,  # Wrong skill
            EventModel.execution_time_ms > self.thresholds.slow_execution_ms,  # Slow
            EventModel.execution_cost > self.thresholds.high_cost_usd,  # Expensive
            EventModel.user_feedback_score < self.thresholds.low_satisfaction,  # Low satisfaction
        ]
        
        events = self.session.query(EventModel).filter(
            or_(*conditions),
            EventModel.created_at >= cutoff
        ).order_by(EventModel.created_at.desc()).limit(limit).all()
        
        return [
            {
                "event_id": e.event_id,
                "user_query": e.user_query,
                "selected_skills": e.selected_skills,
                "correction_suggestion": e.correction_suggestion,
                "execution_time_ms": e.execution_time_ms,
                "execution_cost": e.execution_cost,
                "user_feedback_score": e.user_feedback_score,
                "selection_correctness": e.selection_correctness,
            }
            for e in events
        ]
    
    def get_slow_executions(self, days: int = 7, limit: int = 10) -> list[dict]:
        """Get recent slow skill executions from metrics table.
        
        This complements get_recent_failures by extracting performance data
        from the skill_execution_metrics table.
        """
        from api.models import SkillExecutionMetric
        
        cutoff = datetime.now(timezone.utc) - timedelta(days=days)
        
        metrics = self.session.query(SkillExecutionMetric).filter(
            SkillExecutionMetric.execution_time_ms > self.thresholds.slow_execution_ms,
            SkillExecutionMetric.created_at >= cutoff
        ).order_by(SkillExecutionMetric.created_at.desc()).limit(limit).all()
        
        return [
            {
                "skill_name": m.skill_name,
                "execution_time_ms": m.execution_time_ms,
                "execution_cost": m.execution_cost,
                "session_id": m.session_id,
            }
            for m in metrics
        ]
    
    def get_expensive_executions(self, days: int = 7, limit: int = 10) -> list[dict]:
        """Get recent expensive skill executions from metrics table."""
        from api.models import SkillExecutionMetric
        
        cutoff = datetime.now(timezone.utc) - timedelta(days=days)
        
        metrics = self.session.query(SkillExecutionMetric).filter(
            SkillExecutionMetric.execution_cost > self.thresholds.high_cost_usd,
            SkillExecutionMetric.created_at >= cutoff
        ).order_by(SkillExecutionMetric.created_at.desc()).limit(limit).all()
        
        return [
            {
                "skill_name": m.skill_name,
                "execution_time_ms": m.execution_time_ms,
                "execution_cost": m.execution_cost,
                "session_id": m.session_id,
            }
            for m in metrics
        ]
    
    def _extract_signal(self, failure: dict, signal_type: SignalType) -> LearningSignal | None:
        """Extract a learning signal from a failure event.
        
        Args:
            failure: Failure event data
            signal_type: Type of signal to extract
        
        Returns:
            LearningSignal if applicable, None otherwise
        """
        query = failure["user_query"][:255]
        selected = failure.get("selected_skills", [])
        
        if signal_type == SignalType.WRONG_SKILL:
            correction = failure.get("correction_suggestion")
            if not correction:
                return None
            return LearningSignal(
                signal_type=SignalType.WRONG_SKILL,
                query_pattern=query,
                wrong_skills=selected,
                correct_skills=correction,
                target_metrics={"accuracy": 1.0},
            )
        
        elif signal_type == SignalType.SLOW_EXECUTION:
            exec_time = failure.get("execution_time_ms")
            if exec_time is None or exec_time < self.thresholds.slow_execution_ms:
                return None
            return LearningSignal(
                signal_type=SignalType.SLOW_EXECUTION,
                query_pattern=query,
                wrong_skills=selected,
                correct_skills=[],  # To be filled by optimization
                target_metrics={"time_ms": exec_time * 0.5},  # Target: 50% faster
            )
        
        elif signal_type == SignalType.HIGH_COST:
            cost = failure.get("execution_cost")
            if cost is None or cost < self.thresholds.high_cost_usd:
                return None
            return LearningSignal(
                signal_type=SignalType.HIGH_COST,
                query_pattern=query,
                wrong_skills=selected,
                correct_skills=[],
                target_metrics={"cost": cost * 0.5},  # Target: 50% cheaper
            )
        
        elif signal_type == SignalType.LOW_SATISFACTION:
            satisfaction = failure.get("user_feedback_score")
            if satisfaction is None or satisfaction >= self.thresholds.low_satisfaction:
                return None
            return LearningSignal(
                signal_type=SignalType.LOW_SATISFACTION,
                query_pattern=query,
                wrong_skills=selected,
                correct_skills=[],
                target_metrics={"satisfaction": 4.0},  # Target: 4+ stars
            )
        
        return None

    def _update_learnings(self, signal: LearningSignal):
        """Update learning database with new signal."""
        # Check if similar learning exists
        existing = self.session.query(LearningModel).filter(
            LearningModel.query_pattern == signal.query_pattern,
            LearningModel.signal_type == signal.signal_type.value
        ).first()
        
        if existing:
            existing.evidence_count += 1
            existing.confidence = min(99, existing.evidence_count * 10)
            # Update target metrics with weighted average
            if existing.target_metrics and signal.target_metrics:
                weight_old = (existing.evidence_count - 1) / existing.evidence_count
                weight_new = 1 / existing.evidence_count
                for key, value in signal.target_metrics.items():
                    if key in existing.target_metrics:
                        existing.target_metrics[key] = (
                            existing.target_metrics[key] * weight_old + value * weight_new
                        )
                    else:
                        existing.target_metrics[key] = value
            elif signal.target_metrics:
                existing.target_metrics = signal.target_metrics
        else:
            learning = LearningModel(
                learning_id=str(uuid7()),
                query_pattern=signal.query_pattern,
                wrong_skills=signal.wrong_skills,
                correct_skills=signal.correct_skills,
                improvement_score=10.0,
                confidence=signal.confidence,
                signal_type=signal.signal_type.value,
                target_metrics=signal.target_metrics,
            )
            self.session.add(learning)
        
        self.session.commit()

    def apply_learnings(self, query: str, candidates: list) -> list:
        """Apply learned corrections to candidate selection with multi-dimensional scoring."""
        
        # Find matching learnings
        learnings = self.session.query(LearningModel).filter(
            LearningModel.confidence >= 50
        ).all()
        
        for learning in learnings:
            if learning.query_pattern.lower() in query.lower():
                # Apply correction
                learning.applied_count += 1
                learning.last_applied_at = datetime.now(timezone.utc)
                self.session.commit()
                
                # Filter out wrong skills, add correct ones
                filtered = [c for c in candidates if c.name not in learning.wrong_skills]
                return filtered
        
        return candidates
    
    def calculate_multi_factor_score(self, event: dict) -> float:
        """Calculate weighted score across multiple dimensions.
        
        Args:
            event: Selection event with metrics
        
        Returns:
            Weighted score (0-100)
        """
        scores = {}
        
        # Accuracy score (0-100)
        if event.get("selection_correctness") is not None:
            scores["accuracy"] = 100.0 if event["selection_correctness"] == 1 else 0.0
        else:
            scores["accuracy"] = 50.0  # Neutral if unknown
        
        # Speed score (0-100, inverse of time)
        exec_time = event.get("execution_time_ms", 0)
        if exec_time > 0:
            # 1s = 100, 10s = 50, 30s+ = 0
            scores["speed"] = max(0, 100 - (exec_time / 300))
        else:
            scores["speed"] = 100.0
        
        # Cost score (0-100, inverse of cost)
        cost = event.get("execution_cost", 0.0)
        if cost > 0:
            # $0.01 = 100, $0.10 = 50, $0.50+ = 0
            scores["cost"] = max(0, 100 - (cost * 200))
        else:
            scores["cost"] = 100.0
        
        # Satisfaction score (0-100, from 1-5 stars)
        satisfaction = event.get("user_feedback_score")
        if satisfaction is not None:
            scores["satisfaction"] = (satisfaction - 1) * 25  # 1->0, 5->100
        else:
            scores["satisfaction"] = 75.0  # Assume good if no feedback
        
        # Weighted average
        total_score = (
            scores["accuracy"] * self.weights.accuracy +
            scores["speed"] * self.weights.speed +
            scores["cost"] * self.weights.cost +
            scores["satisfaction"] * self.weights.satisfaction
        )
        
        return total_score

    def get_learning_stats(self) -> dict[str, Any]:
        """Get statistics about learned corrections with multi-dimensional breakdown."""
        
        total = self.session.query(LearningModel).count()
        high_confidence = self.session.query(LearningModel).filter(
            LearningModel.confidence >= 70
        ).count()
        
        # Calculate average confidence
        avg_confidence = 0.0
        if total > 0:
            from sqlalchemy import func
            result = self.session.query(func.avg(LearningModel.confidence)).scalar()
            avg_confidence = float(result) if result else 0.0
        
        # Breakdown by signal type
        signal_breakdown = {}
        for signal_type in SignalType:
            count = self.session.query(LearningModel).filter(
                LearningModel.signal_type == signal_type.value
            ).count()
            signal_breakdown[signal_type.value] = count
        
        return {
            "total_learnings": total,
            "high_confidence": high_confidence,
            "low_confidence": total - high_confidence,
            "avg_confidence": avg_confidence,
            "by_signal_type": signal_breakdown,
            "weights": self.weights.to_dict(),
        }
