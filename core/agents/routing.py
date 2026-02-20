"""Self-improving model routing based on quality and cost.

Design ref: agents-and-orchestration.md §7 "Intelligent Model Routing"

Routes tasks to models based on complexity, historical efficiency, and budget.
Learns from quality scores and costs to improve routing decisions.

Distributed-safe: all state in DB, no in-memory caches.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from enum import Enum
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)


class TaskComplexity(str, Enum):
    """Task complexity levels."""

    SIMPLE = "simple"
    MEDIUM = "medium"
    COMPLEX = "complex"
    CRITICAL = "critical"


@dataclass
class RouteDecision:
    """A routing decision."""

    model: str
    complexity: TaskComplexity
    reason: str
    estimated_cost: float


class ModelRouter:
    """Route tasks to models based on quality and cost.

    Distributed-safe: all state in DB.
    """

    def __init__(self, db: Session, llm_client=None) -> None:
        self.db = db
        self.llm_client = llm_client

    def classify_complexity(self, task_type: str, query: str) -> TaskComplexity:
        """Classify task complexity from query.

        Args:
            task_type: Type of task
            query: User query

        Returns:
            TaskComplexity level
        """
        # Try LLM classification first if available
        if self.llm_client:
            try:
                result = self.llm_client.chat([{
                    "role": "user",
                    "content": f"Classify complexity (simple/medium/complex/critical):\n{query}"
                }])
                response = result.get("content", "").lower()
                for complexity in TaskComplexity:
                    if complexity.value in response:
                        return complexity
            except Exception:
                pass  # Fallback to keyword matching

        # Fallback: keyword matching
        query_lower = query.lower()

        critical_keywords = ["security", "production", "deploy", "delete", "migrate"]
        if any(kw in query_lower for kw in critical_keywords):
            return TaskComplexity.CRITICAL

        complex_keywords = ["refactor", "architecture", "design", "multi-file", "complex"]
        if any(kw in query_lower for kw in complex_keywords):
            return TaskComplexity.COMPLEX

        medium_keywords = ["edit", "fix", "explain", "review"]
        if any(kw in query_lower for kw in medium_keywords):
            return TaskComplexity.MEDIUM

        return TaskComplexity.SIMPLE

    def route(
        self,
        task_type: str,
        query: str,
        available_models: list[str],
        scope_id: str,
    ) -> RouteDecision:
        """Route a task to a model.

        Args:
            task_type: Type of task
            query: User query
            available_models: List of available models (assumed in cost order: expensive first)
            scope_id: Scope for budget checking

        Returns:
            RouteDecision
        """
        complexity = self.classify_complexity(task_type, query)

        # Get historical efficiency for this task type
        efficiency = self._get_efficiency_ranking(task_type)

        # Route based on complexity
        if complexity == TaskComplexity.CRITICAL:
            model = available_models[0]  # Most capable
            reason = "Critical task requires best model"
        elif complexity == TaskComplexity.COMPLEX:
            # Use most efficient model that's capable enough
            model = self._select_by_efficiency(efficiency, available_models, min_quality=4.0)
            reason = "Complex task routed by efficiency"
        elif complexity == TaskComplexity.MEDIUM:
            model = self._select_by_efficiency(efficiency, available_models, min_quality=3.5)
            reason = "Medium task routed by efficiency"
        else:
            # Simple: use cheapest
            model = available_models[-1] if len(available_models) > 1 else available_models[0]
            reason = "Simple task routed to cheapest model"

        estimated_cost = self._estimate_cost(model, task_type)

        logger.info(
            f"Routed {task_type} ({complexity.value}) to {model}: {reason} "
            f"(est. ${estimated_cost})"
        )

        return RouteDecision(
            model=model,
            complexity=complexity,
            reason=reason,
            estimated_cost=estimated_cost,
        )

    def record_quality(
        self,
        task_type: str,
        model: str,
        quality_score: float,
        cost: float,
    ) -> None:
        """Record quality and cost for a task.

        Args:
            task_type: Type of task
            model: Model used
            quality_score: Quality score (0-5)
            cost: Cost in dollars
        """
        from uuid_utils import uuid7

        self.db.execute(
            text(
                "INSERT INTO model_quality_metrics "
                "(metric_id, task_type, model, quality_score, cost, recorded_at) "
                "VALUES (:id, :task_type, :model, :quality, :cost, NOW())"
            ),
            {
                "id": str(uuid7()),
                "task_type": task_type,
                "model": model,
                "quality": quality_score,
                "cost": cost,
            },
        )
        self.db.commit()
        logger.info(f"Quality recorded: {model} on {task_type}: {quality_score}/5 @ ${cost}")

    def _get_efficiency_ranking(self, task_type: str) -> dict[str, float]:
        """Get efficiency ranking (quality/cost) for models on a task type.

        Args:
            task_type: Type of task

        Returns:
            Dict of model → efficiency score
        """
        rows = self.db.execute(
            text(
                "SELECT model, "
                "  AVG(quality_score) as avg_quality, "
                "  AVG(cost) as avg_cost, "
                "  AVG(quality_score) / AVG(cost) as efficiency "
                "FROM model_quality_metrics "
                "WHERE task_type = :task_type "
                "GROUP BY model "
                "ORDER BY efficiency DESC"
            ),
            {"task_type": task_type},
        ).fetchall()

        return {row[0]: float(row[3]) for row in rows}

    def _select_by_efficiency(
        self,
        efficiency: dict[str, float],
        available_models: list[str],
        min_quality: float = 3.5,
    ) -> str:
        """Select model by efficiency, filtering by minimum quality.

        Args:
            efficiency: Dict of model → efficiency
            available_models: Available models
            min_quality: Minimum acceptable quality

        Returns:
            Selected model
        """
        # Get quality for each model
        quality_data = self._get_quality_data()

        # Filter by min_quality and efficiency
        candidates = [
            m for m in available_models
            if m in efficiency and quality_data.get(m, 0) >= min_quality
        ]

        if candidates:
            # Return highest efficiency
            return max(candidates, key=lambda m: efficiency[m])

        # Fallback: most capable
        return available_models[0]

    def _get_quality_data(self) -> dict[str, float]:
        """Get average quality per model."""
        rows = self.db.execute(
            text(
                "SELECT model, AVG(quality_score) as avg_quality "
                "FROM model_quality_metrics "
                "GROUP BY model"
            )
        ).fetchall()

        return {row[0]: float(row[1]) for row in rows}

    def _estimate_cost(self, model: str, task_type: str) -> float:
        """Estimate cost for a model on a task type.

        Args:
            model: Model name
            task_type: Task type

        Returns:
            Estimated cost in dollars
        """
        row = self.db.execute(
            text(
                "SELECT AVG(cost) FROM model_quality_metrics "
                "WHERE model = :model AND task_type = :task_type"
            ),
            {"model": model, "task_type": task_type},
        ).fetchone()

        if row:
            cost = row[0]
            if cost is not None:
                return float(cost)

        # Default estimates
        defaults = {
            "gpt-4": 0.03,
            "gpt-3.5": 0.001,
            "claude-opus": 0.03,
            "claude-sonnet": 0.003,
            "claude-haiku": 0.0003,
        }
        return defaults.get(model, 0.01)
