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
from core.context.embeddings import EmbeddingService
from core.logging_config import get_logger
from core.sandbox import Sandbox
from core.skills.learning_config import (
    RUNTIME_CONFIG_TTL_SECONDS,
    effective_confidence,
    load_runtime_config,
    resolve_weights_for_signal,
)
from core.skills.learning_signals import LearningSignal, SignalType, SignalWeights, SignalThresholds
from core.skills.learning_similarity import (
    context_matches,
    embedding_to_vec_str,
    extract_context_features,
    l2_similarity,
    normalize_confidence,
    parse_embedding,
    pattern_matches,
    semantic_similarity_map,
)

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
        self.sandbox = Sandbox(db=session, account=account)
        self.embedding_service: EmbeddingService | None = None
        self._runtime_config_cache: dict[str, Any] | None = None
        self._runtime_config_loaded_at: datetime | None = None
        self._runtime_config_last_updated_at: datetime | None = None
        self._runtime_config_ttl_seconds = RUNTIME_CONFIG_TTL_SECONDS
        self._ensure_tables()

    def _ensure_tables(self):
        """Ensure learning tables exist and evolve schema if needed."""
        from sqlalchemy import text

        if not hasattr(self.session, "bind") or self.session.bind is None:
            return
        
        # Check if table exists using raw SQL
        result = self.session.execute(
            text("SELECT 1 FROM information_schema.tables WHERE table_name = 'skill_selection_learning' LIMIT 1")
        ).fetchone()
        if not result:
            return
        
        # Get columns using raw SQL to avoid SQLAlchemy type parsing issues
        columns_result = self.session.execute(
            text("SELECT column_name FROM information_schema.columns WHERE table_name = 'skill_selection_learning'")
        ).fetchall()
        columns = {row[0] for row in columns_result}
        
        if "query_embedding" not in columns:
            self.session.execute(
                text("ALTER TABLE skill_selection_learning ADD COLUMN query_embedding TEXT")
            )
        if "context_features" not in columns:
            self.session.execute(
                text("ALTER TABLE skill_selection_learning ADD COLUMN context_features JSON")
            )
        if "is_active" not in columns:
            self.session.execute(
                text("ALTER TABLE skill_selection_learning ADD COLUMN is_active TINYINT(1) DEFAULT 1")
            )
        self.session.commit()

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

        if learned_count > 0:
            self._persist_learning_updates()

        return {
            "learned": learned_count,
            "total_failures": len(failures),
            "signals_by_type": {k.value: v for k, v in signals_by_type.items()},
        }

    def rollback_learnings(self, learning_ids: list[str] | None = None, since: datetime | None = None) -> int:
        """Soft-delete learnings by ID list or creation time. Returns count deactivated."""
        query = self.session.query(LearningModel).filter(LearningModel.is_active == 1)
        if learning_ids:
            query = query.filter(LearningModel.learning_id.in_(learning_ids))
        elif since:
            query = query.filter(LearningModel.created_at >= since)
        else:
            return 0
        count = query.update({"is_active": 0}, synchronize_session="fetch")
        self._persist_learning_updates()
        return count

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
                "session_id": e.session_id,
                "selection_method": e.selection_method,
                "context_snapshot": e.context_snapshot,
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
        context_features = extract_context_features(query)
        
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
                context_features=context_features,
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
                context_features=context_features,
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
                context_features=context_features,
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
                context_features=context_features,
            )
        
        return None

    def _update_learnings(self, signal: LearningSignal):
        """Update learning database with new signal (no commit — caller batches)."""
        # Check if similar learning exists
        existing = self.session.query(LearningModel).filter(
            LearningModel.query_pattern == signal.query_pattern,
            LearningModel.signal_type == signal.signal_type.value
        ).first()
        embedding = self._embed_query(signal.query_pattern)
        
        if existing:
            existing.evidence_count += 1
            existing.confidence = min(99, existing.evidence_count * 10)
            if existing.query_embedding is None and embedding is not None:
                existing.query_embedding = embedding
            if existing.context_features is None and signal.context_features is not None:
                existing.context_features = signal.context_features
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
                query_embedding=embedding,
                wrong_skills=signal.wrong_skills,
                correct_skills=signal.correct_skills,
                improvement_score=10.0,
                confidence=signal.confidence,
                signal_type=signal.signal_type.value,
                target_metrics=signal.target_metrics,
                context_features=signal.context_features,
            )
            self.session.add(learning)

    def apply_learnings(self, query: str, candidates: list) -> list:
        """Apply learned corrections to candidate selection with multi-dimensional scoring."""
        if not candidates:
            return candidates
        if not query:
            return candidates

        query_lower = query.lower()
        query_features = extract_context_features(query)
        query_embedding = self._embed_query(query)

        runtime_config = self._load_runtime_config()
        weights = runtime_config["weights"]
        per_signal_weights = runtime_config["weights_per_signal"]
        decay = runtime_config["decay"]
        similarity_threshold = runtime_config["semantic_similarity_threshold"]

        similarity_map = semantic_similarity_map(self.session, 
            query_embedding,
            similarity_threshold,
            runtime_config["semantic_match_limit"],
        )
        learnings = self.session.query(LearningModel).filter(
            LearningModel.is_active == 1
        ).all()
        matched = []
        semantic_matches = 0
        substring_matches = 0
        for learning in learnings:
            if not learning.query_pattern:
                continue
            if not self._is_high_confidence_value(
                self._effective_confidence(learning, decay, learning.signal_type)
            ):
                continue
            if not context_matches(learning.context_features, query_features):
                continue
            similarity = None
            if similarity_map is not None:
                if learning.learning_id in similarity_map:
                    similarity = similarity_map[learning.learning_id]
            else:
                learning_embedding = parse_embedding(learning.query_embedding)
                if query_embedding is not None and learning_embedding is not None:
                    similarity = l2_similarity(query_embedding, learning_embedding)
            if similarity is not None and similarity >= similarity_threshold:
                matched.append((learning, similarity))
                semantic_matches += 1
                continue
            if pattern_matches(learning.query_pattern.lower(), query_lower):
                matched.append((learning, 1.0))
                substring_matches += 1

        if not matched:
            return candidates
        logger.info(
            "Applied learnings match summary: semantic=%s substring=%s",
            semantic_matches,
            substring_matches,
        )

        matched.sort(
            key=lambda item: (
                item[1],
                self._effective_confidence(item[0], decay, item[0].signal_type),
                item[0].evidence_count or 0,
                item[0].applied_count or 0,
            ),
            reverse=True,
        )
        matched = [item[0] for item in matched[:3]]

        candidate_map = {}
        candidate_scores = {}
        for candidate in candidates:
            name = candidate.name
            candidate_map[name] = candidate
            candidate_scores[name] = self._normalize_confidence(
                getattr(candidate, "confidence", 1.0) or 1.0
            )

        from core.skills.pipeline import SkillCandidate

        applied_learnings = []
        for learning in matched:
            learning_confidence = self._effective_confidence(learning, decay, learning.signal_type)
            weight = self._get_signal_weight(learning.signal_type, weights, per_signal_weights)
            delta = learning_confidence * weight
            if delta <= 0:
                continue

            wrong_skills = learning.wrong_skills or []
            correct_skills = learning.correct_skills or []
            if not wrong_skills and not correct_skills:
                continue
            changed = False

            for skill in wrong_skills:
                if skill in candidate_scores:
                    current_score = candidate_scores[skill]
                    next_score = max(0.0, current_score - delta)
                    if next_score != current_score:
                        candidate_scores[skill] = next_score
                        changed = True

            for skill in correct_skills:
                if skill not in candidate_scores:
                    candidate_map[skill] = SkillCandidate(name=skill)
                    candidate_scores[skill] = min(1.0, delta)
                    changed = True
                candidate_scores[skill] = min(1.0, candidate_scores[skill] + delta)
                changed = True

            if changed:
                learning.applied_count += 1
                learning.last_applied_at = datetime.now(timezone.utc)
                applied_learnings.append(learning)

        if not applied_learnings:
            return candidates

        self._persist_learning_updates()

        scored = [
            (name, score) for name, score in candidate_scores.items() if score > 0.0
        ]
        if not scored:
            return candidates

        scored.sort(key=lambda item: item[1], reverse=True)
        return [candidate_map[name] for name, _ in scored]

    def _embed_query(self, query: str) -> list[float] | None:
        if not query:
            return None
        try:
            if self.embedding_service is None:
                try:
                    self.embedding_service = EmbeddingService(self.session)
                except Exception as exc:
                    logger.warning(f"Embedding service init failed: {exc}")
                    return None
            return self.embedding_service.embed_text(query)
        except Exception as exc:
            logger.warning(f"Embedding generation failed: {exc}")
            return None

    def _normalize_confidence(self, value: float | None) -> float:
        return normalize_confidence(value)

    def _persist_learning_updates(self) -> None:
        try:
            if self.session.in_transaction():
                self.session.flush()
            else:
                self.session.commit()
        except Exception:
            self.session.rollback()
            raise

    def _is_high_confidence(self, value: float | None) -> bool:
        if value is None:
            return False
        if value <= 1.0:
            return value >= 0.5
        return value >= 50.0

    def _is_high_confidence_value(self, normalized_value: float | None) -> bool:
        if normalized_value is None:
            return False
        return normalized_value >= 0.5

    def _get_signal_weight(
        self,
        signal_type: str | None,
        weights: SignalWeights | None = None,
        per_signal: dict[str, Any] | None = None,
    ) -> float:
        active_weights = weights or self.weights
        if per_signal is not None:
            active_weights = resolve_weights_for_signal(signal_type, active_weights, per_signal)
        if signal_type == SignalType.SLOW_EXECUTION.value:
            return active_weights.speed
        if signal_type == SignalType.HIGH_COST.value:
            return active_weights.cost
        if signal_type == SignalType.LOW_SATISFACTION.value:
            return active_weights.satisfaction
        return active_weights.accuracy
    
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
        weights = self._load_runtime_config()["weights"]
        total_score = (
            scores["accuracy"] * weights.accuracy +
            scores["speed"] * weights.speed +
            scores["cost"] * weights.cost +
            scores["satisfaction"] * weights.satisfaction
        )
        
        return total_score

    def get_learning_stats(self) -> dict[str, Any]:
        """Get statistics about learned corrections with multi-dimensional breakdown."""
        runtime_config = self._load_runtime_config()
        decay = runtime_config["decay"]

        learnings = self.session.query(LearningModel).all()
        total = len(learnings)
        effective_confidences = [
            self._effective_confidence(learning, decay, learning.signal_type)
            for learning in learnings
        ]
        high_confidence = sum(1 for c in effective_confidences if c >= 0.7)
        avg_confidence = (
            sum(effective_confidences) / total * 100.0 if total > 0 else 0.0
        )
        
        # Breakdown by signal type
        signal_breakdown = {}
        for signal_type in SignalType:
            count = self.session.query(LearningModel).filter(
                LearningModel.signal_type == signal_type.value
            ).count()
            signal_breakdown[signal_type.value] = count
        
        # Query regression gate results (validates selector changes before deployment)
        # Key metrics: pass_rate (safety), avg_improvement_pct (effectiveness)
        from api.models import SelectorGateResult
        gate_results = self.session.query(SelectorGateResult).all()
        total_gates = len(gate_results)
        passed = sum(1 for g in gate_results if g.verdict == "PASS")
        failed = total_gates - passed
        pass_rate = passed / total_gates if total_gates > 0 else 0.0
        improvements = [g.improvement_pct for g in gate_results if g.improvement_pct is not None]
        avg_improvement_pct = sum(improvements) / len(improvements) if improvements else 0.0
        
        return {
            "total_learnings": total,
            "high_confidence": high_confidence,
            "low_confidence": total - high_confidence,
            "avg_confidence": avg_confidence,
            "by_signal_type": signal_breakdown,
            "learnings": {
                "weights": runtime_config["weights"].to_dict(),
                "weights_per_signal": runtime_config["weights_per_signal"],
                "decay": runtime_config["decay"],
            },
            "regression_gates": {
                "total_gates": total_gates,
                "passed": passed,
                "failed": failed,
                "pass_rate": pass_rate,
                "avg_improvement_pct": avg_improvement_pct,
            },
            "semantic_similarity_threshold": runtime_config["semantic_similarity_threshold"],
            "semantic_match_limit": runtime_config["semantic_match_limit"],
        }

    def _load_runtime_config(self) -> dict[str, Any]:
        result, loaded_at, last_updated = load_runtime_config(
            self.session,
            self.weights,
            cache=self._runtime_config_cache,
            cache_loaded_at=self._runtime_config_loaded_at,
            cache_last_updated_at=self._runtime_config_last_updated_at,
            ttl_seconds=self._runtime_config_ttl_seconds,
        )
        self._runtime_config_cache = result
        self._runtime_config_loaded_at = loaded_at
        self._runtime_config_last_updated_at = last_updated
        return result

    def _effective_confidence(self, learning, decay: dict[str, Any], signal_type: str | None) -> float:
        return effective_confidence(learning, decay, signal_type)
