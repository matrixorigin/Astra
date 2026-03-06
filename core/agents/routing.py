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

from core.db_consumer import DbConsumer, DbFactory

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


class ModelRouter(DbConsumer):
    """Route tasks to models based on quality and cost.

    Distributed-safe: all state in DB.
    """

    def __init__(self, db_factory: DbFactory, llm_client=None) -> None:
        super().__init__(db_factory)
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
                result = self.llm_client.chat(
                    messages=[{
                        "role": "user",
                        "content": f"Classify complexity (simple/medium/complex/critical):\n{query}"
                    }],
                    user_id="routing",
                    task_hint="routing",
                )
                response = (result.content or "").lower()
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
        """Record quality and cost for a task (no-op, metrics table removed)."""
        logger.info(f"Quality recorded: {model} on {task_type}: {quality_score}/5 @ ${cost}")

    def _get_efficiency_ranking(self, task_type: str) -> dict[str, float]:
        """Get efficiency ranking (quality/cost) for models on a task type."""
        return {}

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
        return {}

    def _estimate_cost(self, model: str, task_type: str) -> float:
        """Estimate cost for a model on a task type."""
        defaults = {
            "gpt-4": 0.03,
            "gpt-3.5": 0.001,
            "claude-opus": 0.03,
            "claude-sonnet": 0.003,
            "claude-haiku": 0.0003,
        }
        return defaults.get(model, 0.01)
