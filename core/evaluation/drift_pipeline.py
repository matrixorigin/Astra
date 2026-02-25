"""Drift Auto-Correction Pipeline — detect → confirm → correct → validate.

Ref: trust-and-safety.md §8 "Drift Auto-Correction Pipeline"

Composes existing components into an automated remediation flow:
  DriftDetector  → detect quality drift signals
  DriftCorrector → confirm via replay, correct via prompt optimization / fallback
  (audit)        → every action is a logged event

Can be triggered periodically (cron) or on-demand (API / CLI).
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Any, Callable

from core.evaluation.drift_detector import (
    CorrectionAction,
    DriftCorrector,
    DriftDetector,
    DriftReport,
)
from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class PipelineResult:
    """Full pipeline execution result."""
    signals_detected: int = 0
    signals_confirmed: int = 0
    corrections_applied: int = 0
    actions: list[dict[str, Any]] = field(default_factory=list)
    error: str | None = None


def run_drift_pipeline(db_factory: Callable) -> PipelineResult:
    """Execute the full drift auto-correction pipeline.

    Args:
        db_factory: Callable that returns a fresh SQLAlchemy Session.

    Returns:
        PipelineResult summarising what was detected and corrected.
    """
    db = db_factory()
    try:
        # 1. Detect
        detector = DriftDetector(db)
        signals = detector.detect()
        if not signals:
            return PipelineResult()

        # 2. Build corrector with available dependencies
        regression_gate = _try_build_regression_gate(db)
        prompt_optimizer = _try_build_prompt_optimizer(db)
        corrector = DriftCorrector(
            db_factory=lambda: db,
            regression_gate=regression_gate,
            prompt_optimizer=prompt_optimizer,
        )

        # 3. Confirm + Correct
        report: DriftReport = corrector.confirm_and_correct(signals)

        return PipelineResult(
            signals_detected=len(report.signals),
            signals_confirmed=len(report.confirmed),
            corrections_applied=sum(
                1 for c in report.corrections
                if c.get("action") not in (CorrectionAction.NONE.value, CorrectionAction.ESCALATE_HUMAN.value)
            ),
            actions=report.corrections,
        )
    except Exception as e:
        logger.error("Drift pipeline failed: %s", e, exc_info=True)
        return PipelineResult(error=str(e))
    finally:
        try:
            db.close()
        except Exception:
            pass


_pipeline_lock = threading.Lock()


def run_drift_pipeline_async(db_factory: Callable) -> None:
    """Fire the pipeline in a background thread (non-blocking).

    Only one pipeline runs at a time — concurrent calls are skipped.
    """
    if not _pipeline_lock.acquire(blocking=False):
        logger.info("Drift pipeline already running, skipping")
        return

    def _run():
        try:
            run_drift_pipeline(db_factory)
        finally:
            _pipeline_lock.release()

    thread = threading.Thread(target=_run, daemon=True, name="drift-pipeline")
    thread.start()
    logger.info("Drift pipeline triggered (async)")


# ── Dependency builders ──────────────────────────────────────────


def _try_build_regression_gate(db):
    try:
        from core.evaluation.regression_gate import RegressionGate
        return RegressionGate(db)
    except Exception:
        return None


def _try_build_prompt_optimizer(db):
    try:
        from core.context.prompt_optimizer import PromptOptimizer
        from core.llm.client import LLMClient
        llm = LLMClient(db_factory=lambda: db)
        return PromptOptimizer(lambda: db, llm)
    except Exception:
        return None
