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
import math
from datetime import datetime, timedelta, timezone
from typing import Any

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.models import SkillSelectionEvent as EventModel, SkillSelectionLearning as LearningModel
from core.context.embeddings import EmbeddingService
from core.logging_config import get_logger
from core.sandbox import Sandbox
from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent
from core.skills.learning_signals import LearningSignal, SignalType, SignalWeights, SignalThresholds

logger = get_logger(__name__)

CONFIG_KEY_LEARNING_WEIGHTS = "selector_learning_weights"
CONFIG_KEY_LEARNING_DECAY = "selector_learning_decay"
CONFIG_KEY_SEMANTIC_SIMILARITY = "selector_semantic_similarity_threshold"
RUNTIME_CONFIG_TTL_SECONDS = 30
SEMANTIC_SIMILARITY_THRESHOLD = 0.78


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
        self.embedding_service = EmbeddingService(session)
        self._runtime_config_cache: dict[str, Any] | None = None
        self._runtime_config_loaded_at: datetime | None = None
        self._runtime_config_last_updated_at: datetime | None = None
        self._runtime_config_ttl_seconds = RUNTIME_CONFIG_TTL_SECONDS
        self._ensure_tables()

    def _ensure_tables(self):
        """Ensure learning tables exist and evolve schema if needed."""
        from sqlalchemy import inspect, text

        if not hasattr(self.session, "bind") or self.session.bind is None:
            return
        inspector = inspect(self.session.bind)
        if "skill_selection_learning" not in inspector.get_table_names():
            return
        columns = {col["name"] for col in inspector.get_columns("skill_selection_learning")}
        if "query_embedding" not in columns:
            self.session.execute(
                text("ALTER TABLE skill_selection_learning ADD COLUMN query_embedding JSON")
            )
        if "context_features" not in columns:
            self.session.execute(
                text("ALTER TABLE skill_selection_learning ADD COLUMN context_features JSON")
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
        context_features = self._extract_context_features_from_query(query)
        
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
        """Update learning database with new signal."""
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
        
        self._persist_learning_updates()

    def apply_learnings(self, query: str, candidates: list) -> list:
        """Apply learned corrections to candidate selection with multi-dimensional scoring."""
        if not candidates:
            return candidates
        if not query:
            return candidates

        query_lower = query.lower()
        query_features = self._extract_context_features_from_query(query)
        query_embedding = self._embed_query(query)

        runtime_config = self._load_runtime_config()
        weights = runtime_config["weights"]
        per_signal_weights = runtime_config["weights_per_signal"]
        decay = runtime_config["decay"]
        similarity_threshold = runtime_config["semantic_similarity_threshold"]

        learnings = self.session.query(LearningModel).all()
        matched = []
        for learning in learnings:
            if not learning.query_pattern:
                continue
            if not self._is_high_confidence_value(
                self._effective_confidence(learning, decay, learning.signal_type)
            ):
                continue
            if not self._context_matches(learning.context_features, query_features):
                continue
            similarity = None
            learning_embedding = self._parse_embedding(learning.query_embedding)
            if query_embedding is not None and learning_embedding is not None:
                similarity = self._cosine_similarity(query_embedding, learning_embedding)
                if similarity >= similarity_threshold:
                    matched.append((learning, similarity))
                    continue
            if learning.query_pattern.lower() in query_lower:
                matched.append((learning, 1.0))

        if not matched:
            return candidates

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

        from core.agent.selector import SkillCandidate

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
            return self.embedding_service.embed_text(query)
        except Exception as exc:
            logger.warning(f"Embedding generation failed: {exc}")
            return None

    def _parse_embedding(self, value: Any) -> list[float] | None:
        if value is None:
            return None
        if isinstance(value, list):
            return value
        if isinstance(value, str):
            try:
                parsed = json.loads(value)
            except json.JSONDecodeError:
                return None
            if isinstance(parsed, list):
                return parsed
        return None

    def _cosine_similarity(self, left: list[float], right: list[float]) -> float:
        if not left or not right:
            return 0.0
        if len(left) != len(right):
            return 0.0
        dot = 0.0
        left_norm = 0.0
        right_norm = 0.0
        for i in range(len(left)):
            lval = float(left[i])
            rval = float(right[i])
            dot += lval * rval
            left_norm += lval * lval
            right_norm += rval * rval
        denom = math.sqrt(left_norm) * math.sqrt(right_norm)
        if denom == 0:
            return 0.0
        return dot / denom

    def _extract_context_features_from_query(self, query: str) -> dict[str, Any]:
        length = len(query)
        if length <= 50:
            length_bucket = "short"
        elif length <= 200:
            length_bucket = "medium"
        else:
            length_bucket = "long"
        contains_code = "```" in query or "def " in query or "class " in query or ";" in query
        return {
            "length_bucket": length_bucket,
            "contains_code": contains_code,
        }

    def _context_matches(self, learning_features: dict[str, Any] | None, query_features: dict[str, Any]) -> bool:
        if not learning_features:
            return True
        for key, value in learning_features.items():
            if query_features.get(key) != value:
                return False
        return True

    def _normalize_confidence(self, value: float | None) -> float:
        if value is None:
            return 0.0
        if value <= 1.0:
            return max(0.0, float(value))
        return min(1.0, float(value) / 100.0)

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
            active_weights = self._resolve_weights_for_signal(
                signal_type,
                active_weights,
                per_signal,
            )
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
        
        return {
            "total_learnings": total,
            "high_confidence": high_confidence,
            "low_confidence": total - high_confidence,
            "avg_confidence": avg_confidence,
            "by_signal_type": signal_breakdown,
            "weights": runtime_config["weights"].to_dict(),
            "weights_per_signal": runtime_config["weights_per_signal"],
            "decay": runtime_config["decay"],
        }

    def _load_runtime_config(self) -> dict[str, Any]:
        now = datetime.now(timezone.utc)
        if self._runtime_config_cache and self._runtime_config_loaded_at:
            cache_age = (now - self._runtime_config_loaded_at).total_seconds()
            if cache_age < self._runtime_config_ttl_seconds:
                from api.models import Config
                from sqlalchemy import func

                latest_updated_at = (
                    self.session.query(func.max(Config.updated_at))
                    .filter(
                        Config.key_name.in_(
                            [
                                CONFIG_KEY_LEARNING_WEIGHTS,
                                CONFIG_KEY_LEARNING_DECAY,
                                CONFIG_KEY_SEMANTIC_SIMILARITY,
                            ]
                        )
                    )
                    .scalar()
                )
                if (
                    (latest_updated_at is None and self._runtime_config_last_updated_at is None)
                    or (
                        latest_updated_at
                        and self._runtime_config_last_updated_at
                        and latest_updated_at <= self._runtime_config_last_updated_at
                    )
                ):
                    return self._runtime_config_cache

        weights = self.weights
        per_signal_weights: dict[str, Any] = {}
        decay = {
            "enabled": False,
            "half_life_days": 0.0,
            "min_confidence": 0.0,
            "per_signal": {},
        }
        semantic_similarity_threshold = SEMANTIC_SIMILARITY_THRESHOLD

        from api.models import Config

        configs = (
            self.session.query(Config)
            .filter(
                Config.key_name.in_(
                    [
                        CONFIG_KEY_LEARNING_WEIGHTS,
                        CONFIG_KEY_LEARNING_DECAY,
                        CONFIG_KEY_SEMANTIC_SIMILARITY,
                    ]
                )
            )
            .all()
        )
        latest_updated_at = None
        for cfg in configs:
            if cfg.updated_at and (latest_updated_at is None or cfg.updated_at > latest_updated_at):
                latest_updated_at = cfg.updated_at
            if cfg.key_name == CONFIG_KEY_LEARNING_WEIGHTS:
                parsed = self._parse_json_config(cfg.value)
                if isinstance(parsed, dict):
                    per_signal_weights = self._sanitize_per_signal_weights(
                        parsed.get("per_signal", {}) or {}
                    )
                weights = self._merge_weights(weights, parsed)
            elif cfg.key_name == CONFIG_KEY_LEARNING_DECAY:
                parsed = self._parse_json_config(cfg.value)
                if isinstance(parsed, dict):
                    decay = {
                        "enabled": bool(parsed.get("enabled", False)),
                        "half_life_days": float(parsed.get("half_life_days", 0.0) or 0.0),
                        "min_confidence": float(parsed.get("min_confidence", 0.0) or 0.0),
                        "per_signal": parsed.get("per_signal", {}) or {},
                    }
            elif cfg.key_name == CONFIG_KEY_SEMANTIC_SIMILARITY:
                parsed = self._parse_json_config(cfg.value)
                if isinstance(parsed, dict) and "threshold" in parsed:
                    semantic_similarity_threshold = float(parsed["threshold"])
                elif isinstance(parsed, (int, float, str)):
                    semantic_similarity_threshold = float(parsed)

        self._runtime_config_cache = {
            "weights": weights,
            "weights_per_signal": per_signal_weights,
            "decay": decay,
            "semantic_similarity_threshold": semantic_similarity_threshold,
        }
        self._runtime_config_loaded_at = now
        self._runtime_config_last_updated_at = latest_updated_at
        return self._runtime_config_cache

    def _parse_json_config(self, raw: str | None) -> Any:
        if not raw:
            return None
        try:
            return json.loads(raw)
        except Exception:
            logger.warning("Failed to parse selector runtime config, using defaults")
            return None

    def _merge_weights(self, base: SignalWeights, override: Any) -> SignalWeights:
        if not isinstance(override, dict):
            return base
        merged = base.to_dict()
        for key in ["accuracy", "speed", "cost", "satisfaction"]:
            if key in override:
                merged[key] = float(override[key])
        try:
            return SignalWeights(**merged)
        except (TypeError, ValueError):
            logger.warning("Invalid selector_learning_weights, using defaults")
            return base

    def _sanitize_per_signal_weights(self, per_signal: Any) -> dict[str, dict[str, float]]:
        if not isinstance(per_signal, dict):
            logger.warning("Invalid selector_learning_weights per_signal, using defaults")
            return {}
        valid_signals = {st.value for st in SignalType}
        allowed_keys = {"accuracy", "speed", "cost", "satisfaction"}
        sanitized: dict[str, dict[str, float]] = {}
        for signal_type, override in per_signal.items():
            if signal_type not in valid_signals:
                logger.warning("Unknown signal_type in per_signal weights, skipping")
                continue
            if not isinstance(override, dict):
                logger.warning("Invalid per_signal override for signal_type, skipping")
                continue
            cleaned: dict[str, float] = {}
            for key, value in override.items():
                if key not in allowed_keys:
                    logger.warning("Invalid weight key in per_signal override, skipping")
                    continue
                try:
                    cleaned[key] = float(value)
                except (TypeError, ValueError):
                    logger.warning("Invalid weight value in per_signal override, skipping")
            if cleaned:
                sanitized[signal_type] = cleaned
        return sanitized

    def _effective_confidence(
        self,
        learning: LearningModel,
        decay: dict[str, Any],
        signal_type: str | None,
    ) -> float:
        normalized = self._normalize_confidence(learning.confidence)
        decay_config = self._resolve_decay_config(decay, signal_type)
        if not decay_config.get("enabled") or not decay_config.get("half_life_days"):
            return normalized
        reference = learning.updated_at or learning.created_at
        if not reference:
            return normalized
        if reference.tzinfo is None:
            reference = reference.replace(tzinfo=timezone.utc)
        age_days = (datetime.now(timezone.utc) - reference).total_seconds() / 86400.0
        factor = 0.5 ** (age_days / float(decay_config["half_life_days"]))
        decayed = normalized * factor
        min_confidence = float(decay_config.get("min_confidence", 0.0) or 0.0)
        decayed = max(min_confidence, decayed)
        return min(normalized, decayed)

    def _resolve_weights_for_signal(
        self,
        signal_type: str | None,
        base: SignalWeights,
        per_signal: dict[str, Any],
    ) -> SignalWeights:
        if not signal_type:
            return base
        override = per_signal.get(signal_type)
        if not isinstance(override, dict):
            return base
        merged = base.to_dict()
        for key in ["accuracy", "speed", "cost", "satisfaction"]:
            if key in override:
                merged[key] = float(override[key])
        total = sum(merged.values())
        if total <= 0:
            return base
        if abs(total - 1.0) > 0.01:
            merged = {key: value / total for key, value in merged.items()}
        try:
            return SignalWeights(**merged)
        except (TypeError, ValueError):
            logger.warning("Invalid selector_learning_weights per_signal override, using defaults")
            return base

    def _resolve_decay_config(self, decay: dict[str, Any], signal_type: str | None) -> dict[str, Any]:
        base = {
            "enabled": bool(decay.get("enabled", False)),
            "half_life_days": float(decay.get("half_life_days", 0.0) or 0.0),
            "min_confidence": float(decay.get("min_confidence", 0.0) or 0.0),
        }
        if not signal_type:
            return base
        per_signal = decay.get("per_signal") or {}
        override = per_signal.get(signal_type)
        if not isinstance(override, dict):
            return base
        merged = base.copy()
        if "enabled" in override:
            merged["enabled"] = bool(override["enabled"])
        if "half_life_days" in override:
            merged["half_life_days"] = float(override["half_life_days"] or 0.0)
        if "min_confidence" in override:
            merged["min_confidence"] = float(override["min_confidence"] or 0.0)
        return merged
