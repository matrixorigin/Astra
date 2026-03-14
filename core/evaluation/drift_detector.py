"""Model drift detection — detect and correct quality degradation.

Ref: trust-and-safety.md §8 "Model Drift Detection"

Three-phase pipeline:
  1. DETECT: query quality_score trends per model/template, flag significant drops
  2. CONFIRM: replay golden sessions in clone to eliminate false positives
  3. CORRECT: route affected tasks to fallback model or try prompt variants
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from typing import Any

from sqlalchemy import text

from core.logging_config import get_logger
from core.db_consumer import DbConsumer, DbFactory

logger = get_logger(__name__)


class DriftSeverity(str, Enum):
    NONE = "none"
    MILD = "mild"  # week_delta in [-0.5, -0.3)
    SIGNIFICANT = "significant"  # week_delta in [-1.0, -0.5)
    SEVERE = "severe"  # week_delta < -1.0


class CorrectionAction(str, Enum):
    NONE = "none"
    FALLBACK_MODEL = "fallback_model"
    OPTIMIZE_PROMPT = "optimize_prompt"
    ESCALATE_HUMAN = "escalate_human"


@dataclass
class DriftSignal:
    model: str
    template_id: str | None
    current_avg: float
    previous_avg: float
    week_delta: float
    severity: DriftSeverity
    sample_count: int
    detected_at: datetime


@dataclass
class DriftReport:
    signals: list[DriftSignal]
    confirmed: list[DriftSignal]
    corrections: list[dict[str, Any]]
    created_at: datetime


class DriftDetector(DbConsumer):
    """Detects quality drift per model and prompt template."""

    # Thresholds for severity classification
    MILD_THRESHOLD = -0.3
    SIGNIFICANT_THRESHOLD = -0.5
    SEVERE_THRESHOLD = -1.0
    MIN_SAMPLES = 5  # minimum events to consider

    def __init__(self, db_factory: DbFactory):
        super().__init__(db_factory)

    def detect(self) -> list[DriftSignal]:
        """Detect quality drift across all models and templates.

        Compares avg quality_score of last 7 days vs prior 7 days.
        """
        signals: list[DriftSignal] = []

        # Model-level drift
        signals.extend(self._detect_model_drift())

        # Template-level drift (finer granularity)
        signals.extend(self._detect_template_drift())

        return [s for s in signals if s.severity != DriftSeverity.NONE]

    def _detect_model_drift(self) -> list[DriftSignal]:
        with self._db() as db:
            rows = db.execute(
                text("""
                SELECT
                    llm_model_used,
                    AVG(CASE WHEN created_at > DATE_SUB(NOW(), INTERVAL 7 DAY)
                        THEN quality_score END) AS recent_avg,
                    AVG(CASE WHEN created_at BETWEEN DATE_SUB(NOW(), INTERVAL 14 DAY)
                        AND DATE_SUB(NOW(), INTERVAL 7 DAY)
                        THEN quality_score END) AS previous_avg,
                    COUNT(CASE WHEN created_at > DATE_SUB(NOW(), INTERVAL 7 DAY)
                        THEN 1 END) AS recent_count
                FROM agent_events
                WHERE event_type = 'llm_response'
                  AND quality_score IS NOT NULL
                  AND llm_model_used IS NOT NULL
                  AND created_at > DATE_SUB(NOW(), INTERVAL 14 DAY)
                GROUP BY llm_model_used
            """)
            ).fetchall()

            return [
                self._build_signal(
                    model=row[0],
                    template_id=None,
                    recent_avg=row[1],
                    previous_avg=row[2],
                    sample_count=int(row[3]),
                )
                for row in rows
                if row[1] is not None and row[2] is not None
            ]

    def _detect_template_drift(self) -> list[DriftSignal]:
        with self._db() as db:
            rows = db.execute(
                text("""
                SELECT
                    llm_model_used,
                    prompt_template_id,
                    AVG(CASE WHEN created_at > DATE_SUB(NOW(), INTERVAL 7 DAY)
                        THEN quality_score END) AS recent_avg,
                    AVG(CASE WHEN created_at BETWEEN DATE_SUB(NOW(), INTERVAL 14 DAY)
                        AND DATE_SUB(NOW(), INTERVAL 7 DAY)
                        THEN quality_score END) AS previous_avg,
                    COUNT(CASE WHEN created_at > DATE_SUB(NOW(), INTERVAL 7 DAY)
                        THEN 1 END) AS recent_count
                FROM agent_events
                WHERE event_type = 'llm_response'
                  AND quality_score IS NOT NULL
                  AND llm_model_used IS NOT NULL
                  AND prompt_template_id IS NOT NULL
                  AND created_at > DATE_SUB(NOW(), INTERVAL 14 DAY)
                GROUP BY llm_model_used, prompt_template_id
            """)
            ).fetchall()

            return [
                self._build_signal(
                    model=row[0],
                    template_id=row[1],
                    recent_avg=row[2],
                    previous_avg=row[3],
                    sample_count=int(row[4]),
                )
                for row in rows
                if row[2] is not None and row[3] is not None
            ]

    def _build_signal(
        self,
        *,
        model: str,
        template_id: str | None,
        recent_avg: float,
        previous_avg: float,
        sample_count: int,
    ) -> DriftSignal:
        recent = float(recent_avg)
        previous = float(previous_avg)
        delta = recent - previous
        severity = DriftDetector._classify(delta, sample_count)
        return DriftSignal(
            model=model,
            template_id=template_id,
            current_avg=recent,
            previous_avg=previous,
            week_delta=delta,
            severity=severity,
            sample_count=sample_count,
            detected_at=datetime.now(timezone.utc),
        )

    @staticmethod
    def _classify(delta: float, sample_count: int) -> DriftSeverity:
        if sample_count < DriftDetector.MIN_SAMPLES:
            return DriftSeverity.NONE
        if delta < DriftDetector.SEVERE_THRESHOLD:
            return DriftSeverity.SEVERE
        if delta < DriftDetector.SIGNIFICANT_THRESHOLD:
            return DriftSeverity.SIGNIFICANT
        if delta < DriftDetector.MILD_THRESHOLD:
            return DriftSeverity.MILD
        return DriftSeverity.NONE


class DriftCorrector(DbConsumer):
    """Confirms drift via replay and applies corrections."""

    CONFIRM_REPLAY_COUNT = 20
    CONFIRM_THRESHOLD = -0.3  # confirmed if replay delta < this

    def __init__(
        self, db_factory: DbFactory, regression_gate=None, router=None, prompt_optimizer=None
    ):
        super().__init__(db_factory)
        self.regression_gate = regression_gate
        self.router = router
        self.prompt_optimizer = prompt_optimizer

    def confirm_and_correct(
        self,
        signals: list[DriftSignal],
    ) -> DriftReport:
        """Phase 2+3: confirm signals via replay, then correct."""
        confirmed: list[DriftSignal] = []
        corrections: list[dict[str, Any]] = []

        for signal in signals:
            if self._confirm(signal):
                confirmed.append(signal)
                correction = self._correct(signal)
                corrections.append(correction)
                self._record(signal, correction)

        return DriftReport(
            signals=signals,
            confirmed=confirmed,
            corrections=corrections,
            created_at=datetime.now(timezone.utc),
        )

    def _confirm(self, signal: DriftSignal) -> bool:
        """Replay golden sessions to confirm drift is real."""
        if not self.regression_gate:
            # No gate available — trust the statistical signal
            return signal.severity in (
                DriftSeverity.SIGNIFICANT,
                DriftSeverity.SEVERE,
            )

        try:
            result = self.regression_gate.validate_change(
                change_type="config",
                change_id=f"drift_check_{signal.model}",
                change_content={"model": signal.model},
                golden_session_count=self.CONFIRM_REPLAY_COUNT,
            )
            return result.get("verdict") == "fail"
        except Exception as e:
            logger.warning("Drift confirmation replay failed: %s", e)
            # Conservative: treat significant+ as confirmed
            return signal.severity in (
                DriftSeverity.SIGNIFICANT,
                DriftSeverity.SEVERE,
            )

    def _correct(self, signal: DriftSignal) -> dict[str, Any]:
        """Apply correction based on severity and drift source.

        Priority: template-level drift → prompt optimization first,
        model-level drift → fallback model.
        """
        action = CorrectionAction.NONE

        if signal.severity in (DriftSeverity.SIGNIFICANT, DriftSeverity.SEVERE):
            # Template-level drift: try prompt optimization before fallback
            if signal.template_id and self.prompt_optimizer:
                action = self._try_prompt_optimization(signal)

            # If prompt optimization didn't help or not applicable, fallback model
            if action in (CorrectionAction.NONE, CorrectionAction.ESCALATE_HUMAN):
                action = self._apply_fallback(signal)

        return {
            "model": signal.model,
            "template_id": signal.template_id,
            "severity": signal.severity.value,
            "action": action.value,
            "week_delta": signal.week_delta,
            "corrected_at": datetime.now(timezone.utc).isoformat(),
        }

    def _try_prompt_optimization(self, signal: DriftSignal) -> CorrectionAction:
        """Attempt to fix template drift via prompt optimization."""
        try:
            result = self.prompt_optimizer.optimize(
                template_id=signal.template_id,
                min_cases=2,
            )
            if getattr(result, "activated", False):
                logger.info(
                    "Drift correction via prompt optimization: %s %s → %s",
                    signal.template_id,
                    getattr(result, "old_version", "?"),
                    getattr(result, "new_version", "?"),
                )
                return CorrectionAction.OPTIMIZE_PROMPT
            return CorrectionAction.NONE
        except Exception as e:
            logger.warning("Prompt optimization failed for %s: %s", signal.template_id, e)
            return CorrectionAction.NONE

    def _apply_fallback(self, signal: DriftSignal) -> CorrectionAction:
        """Route affected model to its fallback."""
        with self._db() as db:
            if not self.router:
                logger.warning(
                    "No router available, cannot apply fallback for %s",
                    signal.model,
                )
                return CorrectionAction.ESCALATE_HUMAN

            try:
                cfg = self.router.get(signal.model)
                if cfg and cfg.fallback_to:
                    logger.info(
                        "Drift correction: routing %s → %s (delta=%.2f)",
                        signal.model,
                        cfg.fallback_to,
                        signal.week_delta,
                    )
                    cfg.is_active = False  # disable drifted model
                    db.commit()
                    return CorrectionAction.FALLBACK_MODEL
                return CorrectionAction.ESCALATE_HUMAN
            except Exception as e:
                logger.error("Fallback application failed: %s", e)
                return CorrectionAction.ESCALATE_HUMAN

    def _record(self, signal: DriftSignal, correction: dict[str, Any]):
        """Record drift event for audit trail."""
        with self._db() as db:
            try:
                db.execute(
                    text("""
                    INSERT INTO agent_events
                        (event_id, session_id, user_id, agent_id, agent_version,
                         event_type, content, causal_chain_id, created_at, llm_model_used)
                    VALUES
                        (:event_id, :session_id, :user_id, 'system', '1.0.0',
                         'drift_correction', :content, :chain_id, NOW(), :model)
                """),
                    {
                        # Deterministic event_id: same drift signal → same PK → idempotent re-recording
                        "event_id": f"drift_{signal.model}_{int(signal.detected_at.timestamp())}",
                        "session_id": "system_drift_detection",
                        "user_id": "system",
                        "content": json.dumps(correction, default=str),
                        "chain_id": f"drift_{signal.model}",
                        "model": signal.model,
                    },
                )
                db.commit()
            except Exception as e:
                logger.warning("Failed to record drift correction: %s", e)
                db.rollback()
