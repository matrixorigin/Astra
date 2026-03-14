"""Context budget tuner — closed-loop optimization of token allocation.

Observe: quality_score per task_type + budget utilization from ctx_snapshots
Diagnose: identify task_types where under/over-allocation correlates with low quality
Propose: adjust ratios toward sections with higher utilization in high-quality sessions
Validate: RegressionGate(ChangeType.CONTEXT_BUDGET)
Deploy: write to configs table (read by ContextManager._load_budget_ratios)
"""

import json
from typing import Any

from sqlalchemy import text

from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)

# Sections that can be tuned
_TUNABLE_SECTIONS = ("code", "history", "docs", "logs")


class ContextBudgetTuner(DbConsumer):
    """Tune context budget ratios from quality feedback."""

    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)

    def observe(self, days: int = 14) -> list[dict[str, Any]]:
        """Observe quality per task_type from context snapshots.

        Returns per-task_type stats: avg quality, sample count,
        avg budget utilization per section.
        """
        # Aggregate quality by task_type (no token_budget in GROUP BY)
        with self._db() as db:
            quality_rows = db.execute(
                text("""
                SELECT
                    cs.task_type,
                    AVG(ce.quality_score) AS avg_quality,
                    COUNT(*) AS sample_count
                FROM ctx_snapshots cs
                JOIN agent_events ce ON ce.snapshot_id = cs.context_capture_id
                WHERE ce.quality_score IS NOT NULL
                  AND cs.task_type IS NOT NULL
                  AND ce.created_at > DATE_SUB(NOW(), INTERVAL :days DAY)
                GROUP BY cs.task_type
            """),
                {"days": days},
            ).fetchall()

            stats: dict[str, dict[str, Any]] = {}
            for row in quality_rows:
                stats[row[0]] = {
                    "task_type": row[0],
                    "avg_quality": float(row[1]),
                    "sample_count": int(row[2]),
                    "budgets": [],
                }

            # Collect budget data separately (one row per snapshot)
            if stats:
                budget_rows = db.execute(
                    text("""
                    SELECT cs.task_type, cs.token_budget
                    FROM ctx_snapshots cs
                    WHERE cs.task_type IS NOT NULL
                      AND cs.token_budget IS NOT NULL
                      AND cs.created_at > DATE_SUB(NOW(), INTERVAL :days DAY)
                """),
                    {"days": days},
                ).fetchall()

                for row in budget_rows:
                    task_type = row[0]
                    if task_type in stats and row[1]:
                        budget = json.loads(row[1]) if isinstance(row[1], str) else row[1]
                        stats[task_type]["budgets"].append(budget)

            return list(stats.values())

    def diagnose(self, observations: list[dict[str, Any]]) -> list[dict[str, Any]]:
        """Identify task_types with low quality that might benefit from budget reallocation.

        Returns list of task_types needing adjustment with diagnosis.
        """
        issues = []
        for obs in observations:
            if obs["sample_count"] < 10:
                continue
            if obs["avg_quality"] < 3.5:
                # Analyze budget utilization patterns
                utilization = self._compute_avg_utilization(obs.get("budgets", []))
                issues.append(
                    {
                        "task_type": obs["task_type"],
                        "avg_quality": obs["avg_quality"],
                        "sample_count": obs["sample_count"],
                        "utilization": utilization,
                    }
                )
        return issues

    def propose(self, diagnoses: list[dict[str, Any]]) -> dict[str, dict[str, float]] | None:
        """Propose new budget ratios based on utilization patterns.

        Sections with high utilization (>80%) get more budget;
        sections with low utilization (<30%) give up budget.
        """
        if not diagnoses:
            return None

        from core.context.manager import _BUDGET_RATIOS, TaskType

        proposals: dict[str, dict[str, float]] = {}
        for diag in diagnoses:
            task_type_str = diag["task_type"]
            utilization = diag.get("utilization", {})
            if not utilization:
                continue

            try:
                task_type = TaskType(task_type_str)
            except ValueError:
                continue

            current = dict(_BUDGET_RATIOS[task_type])
            new_ratios = dict(current)

            # Shift budget from underutilized to overutilized sections
            over = [s for s in _TUNABLE_SECTIONS if utilization.get(s, 0) > 0.8 and s in new_ratios]
            under = [
                s for s in _TUNABLE_SECTIONS if utilization.get(s, 0) < 0.3 and s in new_ratios
            ]

            if over and under:
                # Transfer 0.05 per pair
                shift = min(0.05, min(new_ratios[s] for s in under))
                for s in under:
                    new_ratios[s] = max(0.05, new_ratios[s] - shift)
                bonus = shift * len(under) / len(over)
                for s in over:
                    new_ratios[s] = min(0.70, new_ratios[s] + bonus)

                # Normalize to sum=1.0
                total = sum(new_ratios[s] for s in _TUNABLE_SECTIONS if s in new_ratios)
                if total > 0:
                    for s in _TUNABLE_SECTIONS:
                        if s in new_ratios:
                            new_ratios[s] = round(new_ratios[s] / total, 2)

                proposals[task_type_str] = new_ratios

        return proposals if proposals else None

    def validate_and_deploy(self, proposals: dict[str, dict[str, float]]) -> dict[str, Any]:
        """Validate proposals via regression gate, deploy if passed."""
        try:
            from core.evaluation.regression_gate import RegressionGate, ChangeType

            gate = RegressionGate(self._db_factory)
            result = gate.validate_change(
                change_type=ChangeType.CONTEXT_BUDGET,
                change_id="context_budget_ratios",
                change_content=proposals,
                golden_session_count=10,
            )
            verdict = result.get("verdict", "error")
        except Exception as e:
            logger.warning("Gate validation unavailable: %s", e)
            verdict = "skipped"

        if verdict in ("pass", "skip", "skipped"):
            self._deploy(proposals)
            logger.info("Context budget ratios deployed: %s", proposals)
        else:
            logger.warning("Context budget tuning rejected by gate: %s", verdict)

        return {"verdict": verdict, "proposals": proposals}

    def tune(self, days: int = 14) -> dict[str, Any]:
        """Run full observe→diagnose→propose→validate→deploy loop."""
        observations = self.observe(days)
        diagnoses = self.diagnose(observations)
        if not diagnoses:
            return {"status": "no_issues", "observations": len(observations)}

        proposals = self.propose(diagnoses)
        if not proposals:
            return {"status": "no_proposals", "diagnoses": len(diagnoses)}

        return self.validate_and_deploy(proposals)

    def _deploy(self, proposals: dict[str, dict[str, float]]):
        """Write ratios to configs table."""
        with self._db() as db:
            value = json.dumps(proposals)
            try:
                db.execute(
                    text("""
                    INSERT INTO infra_configs (key_name, value, updated_at)
                    VALUES ('context_budget_ratios', :value, NOW())
                    ON DUPLICATE KEY UPDATE value = :value, updated_at = NOW()
                """),
                    {"value": value},
                )
                db.commit()
            except Exception as e:
                logger.error("Failed to deploy budget ratios: %s", e)
                db.rollback()
                raise

    @staticmethod
    def _compute_avg_utilization(budgets: list[dict]) -> dict[str, float]:
        """Compute average utilization (used/allocated) per section."""
        totals: dict[str, list[float]] = {}
        for budget in budgets:
            if not isinstance(budget, dict):
                continue
            for section in _TUNABLE_SECTIONS:
                if section in budget:
                    alloc = budget[section].get("allocated", 0)
                    used = budget[section].get("used", 0)
                    if alloc > 0:
                        totals.setdefault(section, []).append(used / alloc)
        return {s: round(sum(vals) / len(vals), 2) for s, vals in totals.items() if vals}
