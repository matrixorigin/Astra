"""Regression gate for skill selector changes.

Validates that selector changes don't degrade performance on golden queries.
"""

import json
from datetime import datetime, timezone
from typing import Any

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.database import SessionLocal
from core.logging_config import get_logger
from core.sandbox import Sandbox
from core.skills.auditable_selector import AuditableSkillSelector

logger = get_logger(__name__)


class SkillSelectionRegressionGate:
    """Regression gate for skill selector changes."""

    def __init__(self, llm_client, session: Session | None = None, account: str = "sys"):
        self._owns_session = session is None
        self.session = session or SessionLocal()
        self.llm = llm_client
        self.account = account
        self.sandbox = Sandbox(db=self.session, account=account)
        self._ensure_tables()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()

    def close(self):
        if self._owns_session:
            self.session.close()

    def _ensure_tables(self):
        """Ensure gate tables exist - no-op as tables are created by ORM."""
        pass

    def validate_selector_change(
        self,
        new_selector: AuditableSkillSelector,
        old_selector: AuditableSkillSelector,
        test_queries: list[str],
        min_improvement_pct: float = 0.0,
    ) -> dict[str, Any]:
        """Validate that new selector doesn't regress on test queries."""
        
        new_scores = []
        old_scores = []
        
        for query in test_queries:
            try:
                # Test new selector
                new_result = new_selector.select_with_validation(
                    query=query,
                    session_id=f"test_{uuid7()}",
                )
                new_scores.append(1.0 if new_result else 0.0)
                
                # Test old selector
                old_result = old_selector.select_with_validation(
                    query=query,
                    session_id=f"test_{uuid7()}",
                )
                old_scores.append(1.0 if old_result else 0.0)
            except Exception as e:
                logger.error(f"Error testing query '{query}': {e}")
                new_scores.append(0.0)
                old_scores.append(0.0)
        
        new_avg = sum(new_scores) / len(new_scores) if new_scores else 0.0
        old_avg = sum(old_scores) / len(old_scores) if old_scores else 0.0
        improvement = ((new_avg - old_avg) / old_avg * 100) if old_avg > 0 else 0.0
        
        verdict = "pass" if improvement >= min_improvement_pct else "fail"
        
        return {
            "verdict": verdict,
            "new_avg_score": new_avg,
            "old_avg_score": old_avg,
            "improvement_pct": improvement,
            "test_count": len(test_queries),
        }

    def get_golden_queries(self, limit: int = 20) -> list[str]:
        """Get golden test queries from high-quality historical selections."""
        # For now, return empty list - can be populated from historical data
        return []

    def get_gate_history(self, limit: int = 10) -> list[dict]:
        """Get gate validation history."""
        # Return empty list for now
        return []

    def get_gate_stats(self) -> dict[str, Any]:
        """Get gate statistics."""
        from sqlalchemy import text
        
        # Query stats using raw SQL
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
                "total_gates": 0,
                "passed": 0,
                "failed": 0,
                "pass_rate": 0,
                "avg_improvement_pct": 0.0,
            }
        
        total = int(result[0] or 0)
        passed = int(result[1] or 0)
        failed = int(result[2] or 0)
        avg_improvement = float(result[3] or 0)
        
        return {
            "total_gates": total,
            "passed": passed,
            "failed": failed,
            "pass_rate": passed / total if total > 0 else 0,
            "avg_improvement_pct": avg_improvement,
        }
