"""Unified regression gate for prompt/skill/config changes.

Extends replay gating from selector to all versioned inputs.
"""

from __future__ import annotations

import json
import re
from datetime import datetime, timedelta, timezone
from enum import Enum
from typing import Any

from sqlalchemy import func, text
from uuid_utils import uuid7

from api.models.agent import Event
from api.models.evaluation import GateResult
from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger
from core.sandbox import Sandbox

logger = get_logger(__name__)

_SAFE_NAME_RE = re.compile(r"^[a-zA-Z0-9_-]+$")


def _validate_sandbox_name(name: str) -> None:
    """Validate sandbox name to prevent SQL injection in dynamic table references."""
    if not _SAFE_NAME_RE.match(name):
        raise ValueError(
            f"Invalid sandbox name: {name!r}. Only alphanumeric, dash, underscore allowed."
        )


class ChangeType(str, Enum):
    """Type of change being validated"""
    PROMPT = "prompt"
    SKILL = "skill"
    CONFIG = "config"
    SELECTOR = "selector"
    CONTEXT_BUDGET = "context_budget"
    KNOWLEDGE = "knowledge"
    SLO_CRITICAL = "slo_critical"


class RegressionGate(DbConsumer):
    """Unified regression gate for all versioned inputs.

    Validates changes don't degrade quality on golden sessions:
    1. Load golden sessions (high quality_score)
    2. Create snapshot + sandbox
    3. Apply change to sandbox
    4. Replay golden sessions with change
    5. Compute metrics (error rate, score delta, latency, tokens)
    6. Pass/fail decision
    7. Record gate result with lineage
    """

    def __init__(self, db_factory: DbFactory, account: str = "sys"):
        super().__init__(db_factory)
        self.account = account

    def validate_change(
        self,
        change_type: ChangeType,
        change_id: str,
        change_content: dict[str, Any],
        golden_session_count: int = 50,
        error_rate_threshold: float = 0.05,
        score_regression_threshold: float = -0.1,
    ) -> dict[str, Any]:
        """Validate change against golden sessions."""
        from api.services.replay_service import ReplayService

        gate_id = str(uuid7())
        sandbox_name = f"gate_{gate_id[:8]}"

        try:
            # 1. Load golden sessions
            golden_sessions = self._get_golden_sessions(golden_session_count)
            if not golden_sessions:
                logger.warning("No golden sessions found, skipping gate validation")
                return self._build_result(
                    gate_id=gate_id,
                    change_type=change_type,
                    change_id=change_id,
                    verdict="skip",
                    reason="no_golden_sessions_available",
                    sessions_tested=0,
                )

            # 2. Create snapshot + sandbox
            snapshot_id = self._create_snapshot()
            Sandbox(db_factory=self._db_factory, account=self.account).create(
                sandbox_name,
                description=f"Gate {gate_id}",
                created_by="system",
                tables=["ctx_prompt_templates", "skills_registry", "infra_configs",
                         "agent_events", "sk_knowledge_entries"],
            )

            # 3. Apply change to sandbox
            self._apply_change_to_sandbox(
                sandbox_name=sandbox_name,
                change_type=change_type,
                change_id=change_id,
                change_content=change_content,
            )

            # 4. Replay golden sessions
            replay_results = []
            with self._db() as replay_db:
                replay_svc = ReplayService(lambda: replay_db)
                for session in golden_sessions:
                    result = replay_svc.replay_session(
                        session_id=session["session_id"],
                        user_id=session["user_id"],
                        sandbox_name=sandbox_name,
                        mock_mode=True,
                    )
                    replay_results.append({
                        "session_id": session["session_id"],
                        "original_score": session["avg_score"],
                        "replay_status": result["status"],
                        "events_replayed": result["events_replayed"],
                        "successful": result["result"]["successful"],
                        "failed": result["result"]["failed"],
                    })

            # 5. Compute metrics
            metrics = self._compute_metrics(golden_sessions, replay_results)

            # 6. Pass/fail decision
            verdict, reason = self._make_decision(
                metrics=metrics,
                error_rate_threshold=error_rate_threshold,
                score_regression_threshold=score_regression_threshold,
            )

            # 7. Record gate result
            gate_result = self._build_result(
                gate_id=gate_id,
                change_type=change_type,
                change_id=change_id,
                verdict=verdict,
                reason=reason,
                sessions_tested=len(golden_sessions),
                snapshot_id=snapshot_id,
                metrics=metrics,
                replay_results=replay_results,
            )

            self._record_gate_result(gate_result)

            return gate_result

        finally:
            # Cleanup sandbox
            try:
                Sandbox(db_factory=self._db_factory, account=self.account).delete(sandbox_name)
            except Exception as e:
                logger.warning("Failed to cleanup sandbox %s: %s", sandbox_name, e)

    def _get_golden_sessions(self, limit: int) -> list[dict[str, Any]]:
        """Get golden sessions with high quality scores via ORM.

        Selection criteria:
        - quality_score >= 4.0
        - training_eligible = TRUE
        - Multi-turn (event_count >= 3)
        - Recent (last 30 days)

        Uses indexes: idx_events_session_time (session_id, created_at)
        """
        # Python-side cutoff — portable across DB backends and testable.
        cutoff = datetime.now(timezone.utc) - timedelta(days=30)

        with self._db() as db:
            rows = (
                db.query(
                    Event.session_id,
                    Event.user_id,
                    func.avg(Event.quality_score).label("avg_score"),
                    func.count().label("event_count"),
                )
                .filter(
                    Event.quality_score >= 4.0,
                    Event.training_eligible == 1,
                    Event.created_at > cutoff,
                )
                .group_by(Event.session_id, Event.user_id)
                .having(func.count() >= 3)
                .order_by(func.avg(Event.quality_score).desc())
                .limit(limit)
                .all()
            )

            return [
                {
                    "session_id": row.session_id,
                    "user_id": row.user_id,
                    "avg_score": float(row.avg_score),
                    "event_count": int(row.event_count),
                }
                for row in rows
            ]

    def _create_snapshot(self) -> str:
        """Create snapshot of current production state."""
        return f"snapshot_{datetime.now(timezone.utc).strftime('%Y%m%d_%H%M%S')}"

    def _apply_change_to_sandbox(
        self,
        sandbox_name: str,
        change_type: ChangeType,
        change_id: str,
        change_content: dict[str, Any],
    ) -> None:
        """Apply change to sandbox environment.

        Uses raw SQL for sandbox-qualified table names (sandbox_name.table)
        which cannot be expressed via ORM.
        """
        _validate_sandbox_name(sandbox_name)
        with self._db() as db:
            try:
                if change_type == ChangeType.PROMPT:
                    db.execute(text(f"""
                        UPDATE {sandbox_name}.ctx_prompt_templates
                        SET content = :content, updated_at = NOW()
                        WHERE template_id = :template_id
                    """), {
                        "content": change_content.get("content", ""),
                        "template_id": change_content.get("template_id", change_id),
                    })

                elif change_type == ChangeType.SKILL:
                    skill_definition = change_content.get("definition")
                    if skill_definition is None:
                        skill_definition = change_content.get("skill_definition", {})
                    db.execute(text(f"""
                        INSERT INTO {sandbox_name}.skills_registry
                        (skill_id, skill_name, version, description, skill_definition,
                         is_active, created_at, updated_at)
                        VALUES (:skill_id, :skill_name, :version, :description, :definition,
                                1, NOW(), NOW())
                        ON DUPLICATE KEY UPDATE
                        skill_definition = :definition, version = :version,
                        description = :description, is_active = 1, updated_at = NOW()
                    """), {
                        "skill_id": change_id,
                        "skill_name": change_content.get("skill_name") or change_content.get("name", change_id),
                        "version": change_content.get("version", "1.0.0"),
                        "description": change_content.get("description", ""),
                        "definition": skill_definition,
                    })

                elif change_type == ChangeType.CONFIG:
                    db.execute(text(f"""
                        UPDATE {sandbox_name}.configs
                        SET value = :value, updated_at = NOW()
                        WHERE key_name = :key_name
                    """), {
                        "key_name": change_content.get("key", change_id),
                        "value": change_content.get("value", ""),
                    })

                elif change_type == ChangeType.SELECTOR:
                    db.execute(text(f"""
                        UPDATE {sandbox_name}.configs
                        SET value = :value, updated_at = NOW()
                        WHERE key_name = 'selector_config'
                    """), {
                        "value": json.dumps(change_content, default=str),
                    })

                elif change_type == ChangeType.CONTEXT_BUDGET:
                    db.execute(text(f"""
                        INSERT INTO {sandbox_name}.configs (key_name, value, updated_at)
                        VALUES ('context_budget_ratios', :value, NOW())
                        ON DUPLICATE KEY UPDATE value = :value, updated_at = NOW()
                    """), {
                        "value": json.dumps(change_content, default=str),
                    })

                elif change_type == ChangeType.KNOWLEDGE:
                    entry_id = change_content.get("entry_id")
                    if not entry_id:
                        raise ValueError("KNOWLEDGE change requires entry_id")
                    action = change_content.get("action", "quarantine")
                    if action == "quarantine":
                        db.execute(text(f"""
                            UPDATE {sandbox_name}.sk_knowledge_entries
                            SET confidence = 0.0
                            WHERE entry_id = :entry_id
                        """), {"entry_id": entry_id})
                    elif action == "restore":
                        db.execute(text(f"""
                            UPDATE {sandbox_name}.sk_knowledge_entries
                            SET confidence = :confidence
                            WHERE entry_id = :entry_id
                        """), {
                            "entry_id": entry_id,
                            "confidence": change_content.get("confidence", 0.8),
                        })

                elif change_type == ChangeType.SLO_CRITICAL:
                    suspected = change_content.get("suspected_cause")
                    if suspected and suspected.get("change_type") == "skill_version_changed":
                        skill_name = suspected.get("skill_name") or suspected.get("name")
                        version = suspected.get("version")
                        if skill_name and version:
                            db.execute(text(f"""
                                INSERT INTO {sandbox_name}.skills_registry
                                (skill_id, skill_name, version, description, skill_definition,
                                 is_active, created_at, updated_at)
                                SELECT skill_id, skill_name, version, description, skill_definition,
                                       is_active, created_at, updated_at
                                FROM skills_registry
                                WHERE skill_name = :skill_name AND version = :version
                                ON DUPLICATE KEY UPDATE
                                skill_definition = VALUES(skill_definition),
                                is_active = 1, updated_at = NOW()
                            """), {"skill_name": skill_name, "version": version})
                    elif suspected and suspected.get("change_type") == "prompt_template_changed":
                        template_id = suspected.get("template_id")
                        version = suspected.get("version")
                        if template_id and version:
                            db.execute(text(f"""
                                INSERT INTO {sandbox_name}.ctx_prompt_templates
                                (template_id, version, content, created_at)
                                SELECT template_id, version, content, created_at
                                FROM ctx_prompt_templates
                                WHERE template_id = :template_id AND version = :version
                                ON DUPLICATE KEY UPDATE content = VALUES(content)
                            """), {"template_id": template_id, "version": version})

                db.commit()
                logger.info("Applied %s change %s to sandbox %s", change_type, change_id, sandbox_name)

            except Exception as e:
                logger.error("Failed to apply change to sandbox: %s", e)
                db.rollback()
                raise

    def _compute_metrics(
        self,
        golden_sessions: list[dict[str, Any]],
        replay_results: list[dict[str, Any]],
    ) -> dict[str, Any]:
        """Compute gate metrics from replay results."""
        total = len(replay_results)
        if total == 0:
            return {
                "error_rate": 0.0,
                "score_delta": 0.0,
                "avg_original_score": 0.0,
                "avg_replay_score": 0.0,
                "total_sessions": 0,
                "failed_sessions": 0,
            }

        failed = sum(1 for r in replay_results if r["replay_status"] != "completed")
        error_rate = failed / total

        avg_original_score = sum(s["avg_score"] for s in golden_sessions) / total

        replay_scores = []
        for i, result in enumerate(replay_results):
            if result["replay_status"] == "completed" and result["failed"] == 0:
                replay_scores.append(golden_sessions[i]["avg_score"])
            else:
                replay_scores.append(0.0)

        avg_replay_score = sum(replay_scores) / total if replay_scores else 0.0
        score_delta = avg_replay_score - avg_original_score

        return {
            "error_rate": error_rate,
            "score_delta": score_delta,
            "avg_original_score": avg_original_score,
            "avg_replay_score": avg_replay_score,
            "total_sessions": total,
            "failed_sessions": failed,
        }

    def _make_decision(
        self,
        metrics: dict[str, Any],
        error_rate_threshold: float,
        score_regression_threshold: float,
    ) -> tuple[str, str]:
        """Make pass/fail decision based on metrics."""
        if metrics["error_rate"] > error_rate_threshold:
            return "fail", f"error_rate {metrics['error_rate']:.2%} > threshold {error_rate_threshold:.2%}"

        if metrics["score_delta"] < score_regression_threshold:
            return "fail", f"score_delta {metrics['score_delta']:.2f} < threshold {score_regression_threshold:.2f}"

        return "pass", "all_metrics_within_threshold"

    def _build_result(
        self,
        gate_id: str,
        change_type: ChangeType,
        change_id: str,
        verdict: str,
        reason: str,
        sessions_tested: int,
        snapshot_id: str | None = None,
        metrics: dict[str, Any] | None = None,
        replay_results: list[dict[str, Any]] | None = None,
    ) -> dict[str, Any]:
        """Build gate result dict."""
        return {
            "gate_id": gate_id,
            "change_type": change_type.value,
            "change_id": change_id,
            "verdict": verdict,
            "reason": reason,
            "sessions_tested": sessions_tested,
            "snapshot_id": snapshot_id,
            "metrics": metrics or {},
            "replay_results": replay_results or [],
            "created_at": datetime.now(timezone.utc).isoformat(),
        }

    def _record_gate_result(self, gate_result: dict[str, Any]) -> None:
        """Record gate result to database via ORM."""
        with self._db() as db:
            try:
                row = GateResult(
                    gate_id=gate_result["gate_id"],
                    change_type=gate_result["change_type"],
                    change_id=gate_result["change_id"],
                    snapshot_used=gate_result.get("snapshot_id"),
                    sessions_tested=gate_result["sessions_tested"],
                    error_rate=gate_result["metrics"].get("error_rate", 0.0),
                    score_delta=gate_result["metrics"].get("score_delta", 0.0),
                    passed=1 if gate_result["verdict"] == "pass" else 0,
                    metrics=json.dumps(gate_result["metrics"], default=str),
                )
                db.add(row)
                db.commit()
            except Exception as e:
                logger.error("Failed to record gate result: %s", e)
                db.rollback()
                raise

    def get_gate_history(self, limit: int = 10) -> list[dict[str, Any]]:
        """Get gate validation history via ORM."""
        with self._db() as db:
            rows = (
                db.query(GateResult)
                .order_by(GateResult.created_at.desc())
                .limit(limit)
                .all()
            )

            return [
                {
                    "gate_id": r.gate_id,
                    "change_type": r.change_type,
                    "change_id": r.change_id,
                    "snapshot_used": r.snapshot_used,
                    "sessions_tested": r.sessions_tested,
                    "error_rate": float(r.error_rate) if r.error_rate is not None else 0.0,
                    "score_delta": float(r.score_delta) if r.score_delta is not None else 0.0,
                    "passed": bool(r.passed),
                    "metrics": r.metrics,
                    "created_at": r.created_at.isoformat() if r.created_at else None,
                }
                for r in rows
            ]
