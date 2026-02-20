"""Agent SLOs and burn-rate alerting.

Ref: trust-and-safety.md §11

Measures agent effectiveness (not just infra uptime):
  - quality, task completion, hallucination rate, latency, cost efficiency
  - burn-rate alerts when error budget consumption exceeds threshold
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)


class SLOSeverity(str, Enum):
    OK = "ok"
    WARNING = "warning"      # burn rate > 1.5x
    CRITICAL = "critical"    # burn rate > 3x
    BREACH = "breach"        # SLO violated for period


@dataclass
class SLOTarget:
    name: str
    metric: str
    target: float
    operator: str = ">="  # >= or <=


DEFAULT_SLOS = [
    SLOTarget("quality", "avg_quality", 4.0, ">="),
    SLOTarget("hallucination_rate", "hallucination_rate", 0.02, "<="),
    SLOTarget("task_completion", "completion_rate", 0.95, ">="),
]


@dataclass
class SLOStatus:
    slo: SLOTarget
    current_value: float
    met: bool
    burn_rate: float  # 1.0 = on budget, >1 = burning faster
    severity: SLOSeverity
    days_elapsed: int
    bad_days: int


@dataclass
class AgentSLOReport:
    agent_id: str
    statuses: list[SLOStatus]
    period_days: int
    created_at: datetime


class SLOMonitor:
    """Monitors agent-level SLOs with burn-rate alerting."""

    MONTHLY_DAYS = 30
    SLO_COMPLIANCE_TARGET = 0.95  # 95% of days must meet SLO

    def __init__(self, db: Session, slos: list[SLOTarget] | None = None):
        self.db = db
        self.slos = slos or DEFAULT_SLOS

    def check_agent(self, agent_id: str, period_days: int = 30) -> AgentSLOReport:
        """Check all SLOs for an agent over the given period."""
        metrics = self._query_daily_metrics(agent_id, period_days)
        statuses = [self._evaluate_slo(slo, metrics, period_days) for slo in self.slos]

        report = AgentSLOReport(
            agent_id=agent_id,
            statuses=statuses,
            period_days=period_days,
            created_at=datetime.now(timezone.utc),
        )

        # Record any non-OK statuses
        for s in statuses:
            if s.severity != SLOSeverity.OK:
                self._record_alert(agent_id, s)

        return report

    def _query_daily_metrics(
        self, agent_id: str, period_days: int,
    ) -> list[dict[str, Any]]:
        """Query daily aggregated metrics for an agent."""
        try:
            rows = self.db.execute(text("""
                SELECT
                    DATE(created_at) AS day,
                    AVG(quality_score) AS avg_quality,
                    SUM(CASE WHEN event_metadata IS NOT NULL
                        AND JSON_EXTRACT(event_metadata, '$.hallucination_detected') = 'true'
                        THEN 1 ELSE 0 END) * 1.0 / NULLIF(COUNT(*), 0) AS hallucination_rate,
                    COUNT(*) AS total_responses
                FROM conversation_events
                WHERE agent_id = :agent_id
                  AND event_type = 'llm_response'
                  AND created_at >= DATE_SUB(NOW(), INTERVAL :days DAY)
                GROUP BY DATE(created_at)
                ORDER BY day
            """), {"agent_id": agent_id, "days": period_days}).fetchall()

            return [
                {
                    "day": row[0],
                    "avg_quality": float(row[1]) if row[1] else 0.0,
                    "hallucination_rate": float(row[2]) if row[2] else 0.0,
                    "total_responses": int(row[3]),
                    "completion_rate": 0.95,  # TODO: derive from PAOR terminal states
                }
                for row in rows
            ]
        except Exception as e:
            logger.warning("SLO metrics query failed: %s", e)
            return []

    def _evaluate_slo(
        self, slo: SLOTarget, daily_metrics: list[dict[str, Any]],
        period_days: int,
    ) -> SLOStatus:
        """Evaluate a single SLO with burn-rate calculation."""
        if not daily_metrics:
            return SLOStatus(
                slo=slo, current_value=0.0, met=False,
                burn_rate=0.0, severity=SLOSeverity.OK,
                days_elapsed=0, bad_days=0,
            )

        days_elapsed = len(daily_metrics)
        bad_days = 0
        values = []

        for day in daily_metrics:
            val = day.get(slo.metric, 0.0)
            values.append(val)
            if slo.operator == ">=" and val < slo.target:
                bad_days += 1
            elif slo.operator == "<=" and val > slo.target:
                bad_days += 1

        current_value = sum(values) / len(values) if values else 0.0

        # Burn rate: projected bad days vs allowed
        allowed_bad = self.MONTHLY_DAYS * (1 - self.SLO_COMPLIANCE_TARGET)  # 1.5 days
        if days_elapsed > 0 and allowed_bad > 0:
            projected_bad = (bad_days / days_elapsed) * self.MONTHLY_DAYS
            burn_rate = projected_bad / allowed_bad
        else:
            burn_rate = 0.0

        met = (slo.operator == ">=" and current_value >= slo.target) or \
              (slo.operator == "<=" and current_value <= slo.target)

        severity = self._classify_severity(burn_rate, met, days_elapsed, period_days)

        return SLOStatus(
            slo=slo, current_value=round(current_value, 4), met=met,
            burn_rate=round(burn_rate, 2), severity=severity,
            days_elapsed=days_elapsed, bad_days=bad_days,
        )

    @staticmethod
    def _classify_severity(
        burn_rate: float, met: bool, days_elapsed: int, period_days: int,
    ) -> SLOSeverity:
        if days_elapsed >= period_days and not met:
            return SLOSeverity.BREACH
        if burn_rate > 3.0:
            return SLOSeverity.CRITICAL
        if burn_rate > 1.5:
            return SLOSeverity.WARNING
        return SLOSeverity.OK

    def _record_alert(self, agent_id: str, status: SLOStatus):
        """Record SLO alert as auditable event."""
        try:
            import json
            from uuid_extensions import uuid7
            self.db.execute(text("""
                INSERT INTO conversation_events
                    (event_id, session_id, user_id, agent_id, event_type,
                     content, causal_chain_id, created_at)
                VALUES
                    (:eid, 'system_slo', 'system', :aid, 'slo_alert',
                     :content, :chain, NOW())
            """), {
                "eid": str(uuid7()),
                "aid": agent_id,
                "content": json.dumps({
                    "slo": status.slo.name,
                    "severity": status.severity.value,
                    "burn_rate": status.burn_rate,
                    "current_value": status.current_value,
                    "target": status.slo.target,
                    "bad_days": status.bad_days,
                }),
                "chain": f"slo_{agent_id}",
            })
            self.db.commit()
        except Exception as e:
            logger.debug("Failed to record SLO alert: %s", e)
