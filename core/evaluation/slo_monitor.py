"""Agent SLOs and burn-rate alerting with auto-response.

Ref: trust-and-safety.md §11

Measures agent effectiveness (not just infra uptime):
  - quality, task completion, hallucination rate, latency, cost efficiency
  - burn-rate alerts when error budget consumption exceeds threshold

Auto-response (three-tier):
  - warning  → increase monitoring frequency (slo_monitoring_increased event)
  - critical → trigger replay gate asynchronously
  - breach   → create post-mortem event + record model escalation intent
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from typing import TYPE_CHECKING, Any

from sqlalchemy import text

from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

if TYPE_CHECKING:
    from sqlalchemy.orm import Session

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


class SLOMonitor(DbConsumer):
    """Monitors agent-level SLOs with burn-rate alerting."""

    MONTHLY_DAYS = 30
    SLO_COMPLIANCE_TARGET = 0.95  # 95% of days must meet SLO

    def __init__(self, db_factory: DbFactory, slos: list[SLOTarget] | None = None,
                 gate_trigger=None):
        super().__init__(db_factory)
        self.slos = slos or DEFAULT_SLOS
        self._gate_trigger = gate_trigger  # GateTrigger | None

    def check_agent(self, agent_id: str, period_days: int = 30) -> AgentSLOReport:
        """Check all SLOs for an agent over the given period.

        Opens a single session for the entire check: read metrics, evaluate,
        write alert/response events, then batch-commit once.

        If the metrics query fails the report contains zero-data statuses
        (severity=OK, days_elapsed=0) and the error is logged — callers
        can detect this via ``all(s.days_elapsed == 0 for s in report.statuses)``.
        """
        with self._db() as db:
            try:
                metrics = self._query_daily_metrics(db, agent_id, period_days)
            except Exception as e:
                logger.error("SLO metrics query failed for %s: %s", agent_id, e)
                metrics = []

            statuses = [self._evaluate_slo(slo, metrics, period_days) for slo in self.slos]

            report = AgentSLOReport(
                agent_id=agent_id,
                statuses=statuses,
                period_days=period_days,
                created_at=datetime.now(timezone.utc),
            )

            # Record any non-OK statuses and trigger auto-response
            for s in statuses:
                if s.severity != SLOSeverity.OK:
                    self._record_alert(db, agent_id, s)
                    self._auto_respond(db, agent_id, s)

            # Batch commit all SLO events written above
            try:
                db.commit()
            except Exception as e:
                logger.warning("Failed to commit SLO events for %s: %s", agent_id, e)

            return report

    def get_daily_metrics(
        self, agent_id: str, period_days: int,
    ) -> list[dict[str, Any]]:
        """Query daily aggregated metrics for an agent."""
        with self._db() as db:
            return self._query_daily_metrics(db, agent_id, period_days)

    def _query_daily_metrics(
        self, db: "Session", agent_id: str, period_days: int,
    ) -> list[dict[str, Any]]:
        """Query daily aggregated metrics. Caller provides the session.

        Raises on DB errors so the caller can distinguish 'no data' from
        'query failed' — returning [] would silently produce false-OK reports.
        """
        rows = db.execute(text("""
            SELECT
                DATE(created_at) AS day,
                AVG(quality_score) AS avg_quality,
                SUM(CASE WHEN `metadata` IS NOT NULL
                    AND JSON_UNQUOTE(JSON_EXTRACT(`metadata`, '$.hallucination_detected')) = 'true'
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

    def _auto_respond(self, db: "Session", agent_id: str, status: SLOStatus) -> None:
        """Three-tier auto-response to SLO violations.

        warning  → increase monitoring frequency
        critical → trigger replay gate asynchronously
        breach   → post-mortem event + model escalation intent

        Caller provides the session; events are NOT committed here
        so that check_agent can batch-commit all events atomically.
        """
        sev = status.severity
        slo_name = status.slo.name

        if sev == SLOSeverity.WARNING:
            self._write_event(db, agent_id, "slo_monitoring_increased", {
                "slo": slo_name,
                "action": "monitoring_frequency_increased",
                "burn_rate": status.burn_rate,
            })
            logger.info("SLO warning for %s/%s — increased monitoring", agent_id, slo_name)

        elif sev == SLOSeverity.CRITICAL:
            # Trigger replay gate to validate recent changes
            if self._gate_trigger is not None:
                # Find most recent prompt/skill/config change to bind as regression source
                recent_change = self._find_recent_change(db, agent_id)
                change_id = recent_change["change_id"] if recent_change else f"slo_critical:{agent_id}:{slo_name}"
                change_content = {
                    "agent_id": agent_id,
                    "slo": slo_name,
                    "burn_rate": status.burn_rate,
                    "current_value": status.current_value,
                    "suspected_cause": recent_change,  # Link to actual change
                }
                self._gate_trigger.trigger(
                    change_type="slo_critical",
                    change_id=change_id,
                    change_content=change_content,
                )
            # Model escalation intent — same as breach
            self._write_event(db, agent_id, "slo_model_escalation", {
                "slo": slo_name,
                "action": "model_escalation_requested",
                "severity": "critical",
                "burn_rate": status.burn_rate,
            })
            self._write_event(db, agent_id, "slo_gate_triggered", {
                "slo": slo_name,
                "action": "replay_gate_triggered",
                "burn_rate": status.burn_rate,
            })
            logger.warning("SLO critical for %s/%s — gate + model escalation", agent_id, slo_name)

        elif sev == SLOSeverity.BREACH:
            # Post-mortem event
            self._write_event(db, agent_id, "slo_post_mortem", {
                "slo": slo_name,
                "action": "post_mortem_created",
                "current_value": status.current_value,
                "target": status.slo.target,
                "bad_days": status.bad_days,
                "days_elapsed": status.days_elapsed,
            })
            # Model escalation intent — ChatLoop reads this to upgrade model tier
            self._write_event(db, agent_id, "slo_model_escalation", {
                "slo": slo_name,
                "action": "model_escalation_requested",
                "severity": "breach",
                "reason": f"SLO breach: {slo_name} = {status.current_value:.4f} "
                          f"(target {status.slo.operator} {status.slo.target})",
            })
            # HITL policy tightening intent
            self._write_event(db, agent_id, "slo_hitl_tightened", {
                "slo": slo_name,
                "action": "hitl_policy_tightening_requested",
                "reason": "SLO breach requires increased human oversight",
            })
            logger.error(
                "SLO BREACH for %s/%s — post-mortem + escalation + HITL tightening",
                agent_id, slo_name,
            )

    def _find_recent_change(self, db: "Session", agent_id: str) -> dict[str, Any] | None:
        """Find most recent skill/prompt change (global, not agent-specific).
        
        Returns change metadata to bind as suspected regression source.
        Note: skills_registry and prompt_templates are global resources without agent_id,
        so this returns the most recent change across all agents within 7 days.
        """
        try:
            # Find most recent skill change
            skill_row = db.execute(text("""
                SELECT skill_name, version, updated_at
                FROM skills_registry
                WHERE updated_at >= DATE_SUB(NOW(), INTERVAL 7 DAY)
                ORDER BY updated_at DESC
                LIMIT 1
            """)).fetchone()
        
            # Find most recent prompt change
            prompt_row = db.execute(text("""
                SELECT template_id, version, created_at
                FROM prompt_templates
                WHERE created_at >= DATE_SUB(NOW(), INTERVAL 7 DAY)
                ORDER BY created_at DESC
                LIMIT 1
            """)).fetchone()
        
            # Return the most recent of the two
            skill_ts = skill_row[2] if skill_row else None
            prompt_ts = prompt_row[2] if prompt_row else None
        
            if skill_ts and (not prompt_ts or skill_ts > prompt_ts):
                return {
                    "change_type": "skill_version_changed",
                    "change_id": f"{skill_row[0]}@{skill_row[1]}",
                    "skill_name": skill_row[0],
                    "version": skill_row[1],
                    "timestamp": skill_ts.isoformat(),
                }
            elif prompt_ts:
                return {
                    "change_type": "prompt_template_changed",
                    "change_id": f"{prompt_row[0]}@{prompt_row[1]}",
                    "template_id": prompt_row[0],
                    "version": prompt_row[1],
                    "timestamp": prompt_ts.isoformat(),
                }
            return None
        except Exception as e:
            logger.warning("Failed to query recent changes: %s", e)
            return None

    def _write_event(self, db: "Session", agent_id: str, event_type: str, payload: dict[str, Any]) -> None:
        """Write an auditable system event. Does NOT commit — caller batches.

        On failure the session is rolled back so subsequent writes on the
        same session are not poisoned by a dirty transaction state.
        """
        try:
            from core.utils.id_generator import generate_id
            eid = generate_id()
            db.execute(text("""
                INSERT INTO conversation_events
                    (event_id, session_id, user_id, agent_id, agent_version,
                     event_type, content, causal_chain_id, created_at)
                VALUES
                    (:eid, 'system_slo', 'system', :aid, '1.0.0',
                     :etype, :content, :eid, NOW())
            """), {
                "eid": eid,
                "aid": agent_id,
                "etype": event_type,
                "content": json.dumps(payload),
            })
        except Exception as e:
            db.rollback()
            logger.warning("Failed to write SLO event %s: %s", event_type, e)

    def _record_alert(self, db: "Session", agent_id: str, status: SLOStatus):
        """Record SLO alert as auditable event."""
        self._write_event(db, agent_id, "slo_alert", {
            "slo": status.slo.name,
            "severity": status.severity.value,
            "burn_rate": status.burn_rate,
            "current_value": status.current_value,
            "target": status.slo.target,
            "bad_days": status.bad_days,
        })
