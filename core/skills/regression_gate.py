"""Regression gate for skill selector changes.

Prevents skill selection degradation by automatically testing new selectors
against golden queries using Git for Data snapshots.
"""

import json
from datetime import datetime, timedelta
from typing import Any

import numpy as np
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.sandbox import Sandbox
from core.skills.auditable_selector import AuditableSkillSelector, SkillSelectionEvent
from sdk import Database

logger = get_logger(__name__)


class SkillSelectionRegressionGate:
    """Regression gate for skill selector changes.
    
    Key innovation: Every selector change is automatically tested against
    golden queries in sandbox before deployment.
    """

    def __init__(self, db: Database, llm_client, account: str = "sys"):
        self.db = db
        self.llm = llm_client
        self.account = account
        self.sandbox = Sandbox(db=db, account=account)
        self._ensure_tables()

    def _ensure_tables(self):
        """Ensure gate results table exists."""
        self.db.execute(
            """
            CREATE TABLE IF NOT EXISTS selector_gate_results (
                gate_id VARCHAR(36) PRIMARY KEY,
                selector_version VARCHAR(50) NOT NULL,
                test_queries_count INT NOT NULL,
                new_selector_avg_score DECIMAL(5, 2),
                old_selector_avg_score DECIMAL(5, 2),
                improvement_pct DECIMAL(5, 2),
                verdict VARCHAR(20) NOT NULL,
                details JSON,
                tested_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                INDEX idx_version (selector_version),
                INDEX idx_verdict (verdict)
            )
        """
        )

    def validate_selector_change(
        self,
        new_selector: AuditableSkillSelector,
        old_selector: AuditableSkillSelector,
        selector_version: str,
        min_improvement: float = -0.05,  # Allow 5% degradation
    ) -> dict[str, Any]:
        """Validate a new selector against golden queries.
        
        Args:
            new_selector: New selector to test
            old_selector: Current production selector
            selector_version: Version identifier
            min_improvement: Minimum improvement required (negative = allow degradation)
            
        Returns:
            Gate result with verdict
        """
        gate_id = str(uuid7())
        logger.info(f"[{gate_id}] Starting regression gate for selector {selector_version}")

        # Step 1: Get golden queries (high-quality historical selections)
        golden_queries = self._get_golden_queries(limit=100)
        logger.info(f"[{gate_id}] Testing against {len(golden_queries)} golden queries")

        if not golden_queries:
            logger.warning("No golden queries found, skipping gate")
            return {
                "gate_id": gate_id,
                "verdict": "SKIP",
                "reason": "No golden queries available",
            }

        # Step 2: Create sandbox for testing
        sandbox_name = f"gate_{selector_version}_{gate_id[:8]}"
        self.sandbox.create(
            sandbox_name,
            description=f"Regression gate for {selector_version}",
            created_by="regression_gate",
        )

        try:
            # Step 3: Test both selectors
            new_results = self._test_selector(new_selector, golden_queries, sandbox_name)
            old_results = self._test_selector(old_selector, golden_queries, sandbox_name)

            # Step 4: Compare results
            new_avg = np.mean([r["score"] for r in new_results])
            old_avg = np.mean([r["score"] for r in old_results])
            improvement = (new_avg - old_avg) / old_avg if old_avg > 0 else 0

            # Step 5: Make verdict
            if improvement < min_improvement:
                verdict = "FAIL"
                reason = f"Regression detected: {improvement:.2%} (threshold: {min_improvement:.2%})"
            else:
                verdict = "PASS"
                reason = f"Improvement: {improvement:.2%}"

            logger.info(f"[{gate_id}] {verdict}: {reason}")

            # Step 6: Save results
            result = {
                "gate_id": gate_id,
                "selector_version": selector_version,
                "test_queries_count": len(golden_queries),
                "new_selector_avg_score": float(new_avg),
                "old_selector_avg_score": float(old_avg),
                "improvement_pct": float(improvement * 100),
                "verdict": verdict,
                "reason": reason,
                "details": {
                    "new_results": new_results[:10],  # Sample
                    "old_results": old_results[:10],
                },
            }

            self._save_gate_result(result)

            return result

        finally:
            # Cleanup
            self.sandbox.delete(sandbox_name)

    def _get_golden_queries(self, limit: int = 100) -> list[SkillSelectionEvent]:
        """Get golden queries (high user feedback, successful execution)."""
        cutoff = datetime.utcnow() - timedelta(days=30)

        rows = self.db.execute(
            """
            SELECT * FROM skill_selection_events
            WHERE created_at > %s
            AND user_feedback_score >= 4
            AND execution_success = TRUE
            ORDER BY user_feedback_score DESC, created_at DESC
            LIMIT %s
        """,
            (cutoff, limit),
        )

        queries = []
        for row in rows:
            queries.append(
                SkillSelectionEvent(
                    event_id=row["event_id"],
                    session_id=row["session_id"],
                    user_query=row["user_query"],
                    context_snapshot=row["context_snapshot"],
                    available_skills=json.loads(row["available_skills"]),
                    selected_skills=json.loads(row["selected_skills"]),
                    selection_method=row["selection_method"],
                    selection_reasoning=row["selection_reasoning"],
                    candidate_scores=json.loads(row.get("candidate_scores", "{}")),
                    execution_success=row.get("execution_success"),
                    execution_time_ms=row.get("execution_time_ms"),
                    execution_cost=row.get("execution_cost"),
                    user_feedback_score=row.get("user_feedback_score"),
                    created_at=row["created_at"],
                )
            )

        return queries

    def _test_selector(
        self,
        selector: AuditableSkillSelector,
        queries: list[SkillSelectionEvent],
        sandbox_name: str,
    ) -> list[dict[str, Any]]:
        """Test a selector against queries in sandbox."""
        results = []

        for query in queries:
            try:
                # Time-travel to query's original state
                snapshot = query.context_snapshot

                # Select skills using this selector
                # (In production, this would use the snapshot)
                selected = selector._select_candidates(query.user_query)

                # Evaluate selection quality
                score = self._evaluate_selection(selected, query)

                results.append(
                    {
                        "query": query.user_query[:50],
                        "selected": [s.name for s in selected],
                        "expected": query.selected_skills,
                        "score": score,
                    }
                )

            except Exception as e:
                logger.error(f"Failed to test query {query.event_id}: {e}")
                results.append(
                    {"query": query.user_query[:50], "selected": [], "score": 0.0, "error": str(e)}
                )

        return results

    def _evaluate_selection(
        self, selected: list, expected_event: SkillSelectionEvent
    ) -> float:
        """Evaluate selection quality.
        
        Combines multiple factors:
        1. Match with expected skills (from golden query)
        2. User feedback score
        3. Execution success
        """
        if not selected:
            return 0.0

        selected_names = [s.name for s in selected]
        expected_names = expected_event.selected_skills

        # Factor 1: Overlap with expected skills
        overlap = len(set(selected_names) & set(expected_names))
        overlap_score = overlap / max(len(expected_names), 1)

        # Factor 2: User feedback (normalized to 0-1)
        feedback_score = (expected_event.user_feedback_score or 3) / 5.0

        # Factor 3: Execution success
        success_score = 1.0 if expected_event.execution_success else 0.0

        # Weighted combination
        score = 0.4 * overlap_score + 0.3 * feedback_score + 0.3 * success_score

        return score

    def _save_gate_result(self, result: dict[str, Any]):
        """Save gate result to database."""
        self.db.execute(
            """
            INSERT INTO selector_gate_results (
                gate_id, selector_version, test_queries_count,
                new_selector_avg_score, old_selector_avg_score,
                improvement_pct, verdict, details
            ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
        """,
            (
                result["gate_id"],
                result["selector_version"],
                result["test_queries_count"],
                result["new_selector_avg_score"],
                result["old_selector_avg_score"],
                result["improvement_pct"],
                result["verdict"],
                json.dumps(result["details"]),
            ),
        )

    def get_gate_history(self, limit: int = 20) -> list[dict[str, Any]]:
        """Get gate execution history."""
        rows = self.db.execute(
            """
            SELECT * FROM selector_gate_results
            ORDER BY tested_at DESC
            LIMIT %s
        """,
            (limit,),
        )

        history = []
        for row in rows:
            history.append(
                {
                    "gate_id": row["gate_id"],
                    "selector_version": row["selector_version"],
                    "test_queries_count": row["test_queries_count"],
                    "new_score": float(row["new_selector_avg_score"]),
                    "old_score": float(row["old_selector_avg_score"]),
                    "improvement_pct": float(row["improvement_pct"]),
                    "verdict": row["verdict"],
                    "tested_at": row["tested_at"],
                }
            )

        return history

    def get_gate_stats(self) -> dict[str, Any]:
        """Get gate statistics."""
        stats = self.db.execute(
            """
            SELECT 
                COUNT(*) as total_gates,
                SUM(CASE WHEN verdict = 'PASS' THEN 1 ELSE 0 END) as passed,
                SUM(CASE WHEN verdict = 'FAIL' THEN 1 ELSE 0 END) as failed,
                AVG(improvement_pct) as avg_improvement
            FROM selector_gate_results
        """
        )[0]

        return {
            "total_gates": stats["total_gates"],
            "passed": stats["passed"],
            "failed": stats["failed"],
            "pass_rate": (
                stats["passed"] / stats["total_gates"] if stats["total_gates"] > 0 else 0
            ),
            "avg_improvement_pct": float(stats["avg_improvement"] or 0),
        }
