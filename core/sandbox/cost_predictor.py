"""Cost-Aware Branching: predict experiment costs before execution.

Estimates the cost of replay/branch operations using historical LLM call
data, and suggests cheaper alternatives when budget is exceeded.

Usage:
    predictor = BranchCostPredictor(db, model_router)
    estimate = predictor.estimate_replay(session_count=50, model="gpt-4o")
    if estimate.exceeds_budget:
        print(estimate.alternatives)
"""

import logging
from dataclasses import dataclass, field
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

logger = logging.getLogger(__name__)

# Fallback token estimates when no historical data
_DEFAULT_AVG_TOKENS_PER_SESSION = 3000
_DEFAULT_AVG_TURNS_PER_SESSION = 8


@dataclass
class CostEstimate:
    """Result of a cost prediction."""
    operation: str
    model: str
    session_count: int
    estimated_tokens: int
    estimated_cost: float
    budget_remaining: float | None = None
    exceeds_budget: bool = False
    alternatives: list[dict[str, Any]] = field(default_factory=list)


class BranchCostPredictor:
    """Predict costs for branch/replay operations using historical data."""

    def __init__(self, db: Session, model_router=None):
        self.db = db
        self.router = model_router

    def _get_historical_avg_tokens(self, model: str | None = None) -> int:
        """Get average tokens per session from historical LLM call data."""
        try:
            sql = text("""
                SELECT AVG(total_tokens) as avg_tokens
                FROM llm_call_logs
                WHERE total_tokens > 0
            """)
            result = self.db.execute(sql).scalar()
            return int(result) if result else _DEFAULT_AVG_TOKENS_PER_SESSION
        except Exception:
            return _DEFAULT_AVG_TOKENS_PER_SESSION

    def _get_historical_avg_turns(self) -> int:
        """Get average turns per session from historical data."""
        try:
            sql = text("""
                SELECT AVG(cnt) FROM (
                    SELECT session_id, COUNT(*) as cnt
                    FROM conversation_events
                    WHERE event_type IN ('user_query', 'llm_response')
                    GROUP BY session_id
                ) sub
            """)
            result = self.db.execute(sql).scalar()
            return int(result) if result else _DEFAULT_AVG_TURNS_PER_SESSION
        except Exception:
            return _DEFAULT_AVG_TURNS_PER_SESSION

    def estimate_replay(
        self,
        session_count: int,
        model: str,
        budget_remaining: float | None = None,
        avg_tokens_override: int | None = None,
    ) -> CostEstimate:
        """Estimate cost of replaying N sessions.

        Args:
            session_count: Number of sessions to replay
            model: Model to use for replay
            budget_remaining: Optional budget cap
            avg_tokens_override: Override historical avg tokens per session
        """
        avg_tokens = avg_tokens_override or self._get_historical_avg_tokens(model)
        avg_turns = self._get_historical_avg_turns()
        total_tokens = session_count * avg_tokens * avg_turns

        cost = self._estimate_model_cost(model, total_tokens)

        estimate = CostEstimate(
            operation="replay",
            model=model,
            session_count=session_count,
            estimated_tokens=total_tokens,
            estimated_cost=cost,
            budget_remaining=budget_remaining,
        )

        if budget_remaining is not None and cost > budget_remaining:
            estimate.exceeds_budget = True
            estimate.alternatives = self._suggest_alternatives(
                total_tokens, budget_remaining, session_count,
            )

        return estimate

    def estimate_branch(
        self,
        operation: str,
        model: str,
        session_count: int = 1,
        budget_remaining: float | None = None,
    ) -> CostEstimate:
        """Estimate cost of a branch operation (create/diff/merge).

        Branch create/delete are free (zero-copy in MatrixOne).
        Diff and merge may trigger LLM calls for conflict resolution.
        """
        if operation in ("create", "delete"):
            return CostEstimate(
                operation=operation, model=model, session_count=session_count,
                estimated_tokens=0, estimated_cost=0.0,
            )

        # Diff/merge: estimate based on potential conflict resolution LLM calls
        avg_tokens = self._get_historical_avg_tokens(model)
        # Assume 10% of sessions have conflicts needing LLM resolution
        conflict_sessions = max(1, int(session_count * 0.1))
        total_tokens = conflict_sessions * avg_tokens

        cost = self._estimate_model_cost(model, total_tokens)

        estimate = CostEstimate(
            operation=operation, model=model, session_count=session_count,
            estimated_tokens=total_tokens, estimated_cost=cost,
            budget_remaining=budget_remaining,
        )

        if budget_remaining is not None and cost > budget_remaining:
            estimate.exceeds_budget = True
            estimate.alternatives = self._suggest_alternatives(
                total_tokens, budget_remaining, session_count,
            )

        return estimate

    def _estimate_model_cost(self, model: str, total_tokens: int) -> float:
        """Estimate cost using ModelRouter or fallback pricing."""
        if self.router:
            return self.router.estimate_cost(model, total_tokens)
        # Fallback: rough pricing per 1K tokens
        fallback_prices = {
            "gpt-4o": 0.005, "gpt-4o-mini": 0.00015,
            "claude-sonnet-4-20250514": 0.006, "claude-haiku-3.5": 0.001,
        }
        price = fallback_prices.get(model, 0.003)
        return round(total_tokens * price / 1000, 6)

    def _suggest_alternatives(
        self, total_tokens: int, budget: float, session_count: int,
    ) -> list[dict[str, Any]]:
        """Suggest cheaper alternatives when budget is exceeded."""
        alternatives = []

        # Alternative 1: cheaper model
        for alt_model, label in [
            ("gpt-4o-mini", "GPT-4o Mini"), ("claude-haiku-3.5", "Claude Haiku"),
        ]:
            alt_cost = self._estimate_model_cost(alt_model, total_tokens)
            if alt_cost <= budget:
                alternatives.append({
                    "strategy": "cheaper_model",
                    "model": alt_model,
                    "label": label,
                    "estimated_cost": alt_cost,
                    "savings_pct": round((1 - alt_cost / max(budget, 0.001)) * 100, 1),
                })

        # Alternative 2: reduce session count
        if session_count > 1:
            cost_per_session = self._estimate_model_cost(
                "gpt-4o", total_tokens // session_count,
            )
            if cost_per_session > 0:
                max_sessions = int(budget / cost_per_session)
                if 0 < max_sessions < session_count:
                    alternatives.append({
                        "strategy": "reduce_sessions",
                        "session_count": max_sessions,
                        "estimated_cost": round(max_sessions * cost_per_session, 6),
                    })

        return alternatives
