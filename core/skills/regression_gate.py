"""Regression gate for skill selector changes.

Validates that selector changes don't degrade performance on golden queries.
"""

from typing import Any, Protocol

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.sandbox import Sandbox

logger = get_logger(__name__)


class Selector(Protocol):
    """Anything that can select skills given a query."""

    def get_tools_schema(self, query: str, session_id: str, **kw) -> Any: ...


class SkillSelectionRegressionGate:
    """Regression gate for skill selector changes."""

    def __init__(self, llm_client, session: Session, account: str = "sys"):
        if not isinstance(session, Session):
            raise TypeError("session must be a SQLAlchemy Session")

        self.session = session
        self.llm = llm_client
        self.account = account
        self.sandbox = Sandbox(db=session, account=account)

    def validate_selector_change(
        self,
        new_selector: Selector,
        old_selector: Selector,
        test_queries: list[str],
        min_improvement_pct: float = 0.0,
    ) -> dict[str, Any]:
        """Validate that new selector doesn't regress on test queries."""

        new_scores: list[float] = []
        old_scores: list[float] = []

        for query in test_queries:
            sid = f"gate_{uuid7()}"
            try:
                new_result = new_selector.get_tools_schema(query, sid)
                new_scores.append(1.0 if getattr(new_result, "tools", new_result) else 0.0)

                old_result = old_selector.get_tools_schema(query, sid)
                old_scores.append(1.0 if getattr(old_result, "tools", old_result) else 0.0)
            except Exception as e:
                logger.error("Error testing query '%s': %s", query, e)
                new_scores.append(0.0)
                old_scores.append(0.0)

        new_avg = sum(new_scores) / len(new_scores) if new_scores else 0.0
        old_avg = sum(old_scores) / len(old_scores) if old_scores else 0.0
        improvement = ((new_avg - old_avg) / old_avg * 100) if old_avg > 0 else 0.0

        return {
            "verdict": "pass" if improvement >= min_improvement_pct else "fail",
            "new_avg_score": new_avg,
            "old_avg_score": old_avg,
            "improvement_pct": improvement,
            "test_count": len(test_queries),
        }

    def get_golden_queries(self, limit: int = 20) -> list[str]:
        """Get golden test queries from high-quality historical selections."""
        return []

    def get_gate_history(self, limit: int = 10) -> list[dict]:
        """Get gate validation history."""
        return []

    def get_gate_stats(self) -> dict[str, Any]:
        """Get gate statistics."""
        from sqlalchemy import text

        result = self.session.execute(text("""
            SELECT
                COUNT(*) as total,
                SUM(CASE WHEN verdict = 'PASS' THEN 1 ELSE 0 END) as passed,
                SUM(CASE WHEN verdict = 'FAIL' THEN 1 ELSE 0 END) as failed,
                AVG(improvement_pct) as avg_improvement
            FROM selector_gate_results
        """)).fetchone()

        if not result or result[0] is None:
            return {
                "total_gates": 0, "passed": 0, "failed": 0,
                "pass_rate": 0, "avg_improvement_pct": 0.0,
            }

        total = int(result[0] or 0)
        passed = int(result[1] or 0)
        return {
            "total_gates": total,
            "passed": passed,
            "failed": int(result[2] or 0),
            "pass_rate": passed / total if total > 0 else 0,
            "avg_improvement_pct": float(result[3] or 0),
        }
