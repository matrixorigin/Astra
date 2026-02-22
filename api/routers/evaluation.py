"""Evaluation API — quality trends, drift detection, gate history, calibration, closed-loop actions."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Depends, Query
from pydantic import BaseModel, Field
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import SessionLocal, get_db_session
from api.dependencies import get_current_user
from core.logging_config import get_logger

logger = get_logger(__name__)

router = APIRouter(prefix="/api/v1/evaluation")


# ---------------------------------------------------------------------------
# Response models
# ---------------------------------------------------------------------------

class QualityTrendPoint(BaseModel):
    date: str
    avg_score: float
    count: int
    model: str | None = None


class QualityTrendResponse(BaseModel):
    points: list[QualityTrendPoint]
    overall_avg: float
    total_events: int


class DriftSignalResponse(BaseModel):
    model: str
    template_id: str | None
    current_avg: float
    previous_avg: float
    delta: float
    severity: str
    sample_count: int


class GateResultResponse(BaseModel):
    gate_id: str
    change_type: str
    change_id: str
    sessions_tested: int
    error_rate: float
    score_delta: float
    passed: bool
    created_at: str | None


class CalibrationResponse(BaseModel):
    mean_confidence: float
    mean_quality: float
    calibration_error: float
    bias: float
    sample_count: int
    adjustment_multiplier: float
    adjustment_reason: str


class SessionScoreResponse(BaseModel):
    session_id: str
    score: float
    chain_count: int


class GateValidateRequest(BaseModel):
    change_type: str = Field(..., pattern="^(prompt|skill|config|selector|context_budget|knowledge)$")
    change_id: str
    change_content: dict[str, Any]
    golden_session_count: int = Field(default=50, ge=1, le=500)
    error_rate_threshold: float = Field(default=0.05, ge=0.0, le=1.0)
    score_regression_threshold: float = Field(default=-0.1, ge=-5.0, le=0.0)


class GateValidateResponse(BaseModel):
    gate_id: str
    verdict: str
    reason: str
    sessions_tested: int
    metrics: dict[str, Any]


class DriftPipelineResponse(BaseModel):
    signals_detected: int
    signals_confirmed: int
    corrections_applied: int
    actions: list[dict[str, Any]]
    error: str | None = None


class LoopDiagnosisItem(BaseModel):
    input_face: str
    bottleneck: str
    applied: bool
    gate_verdict: str
    error: str | None = None


class ClosedLoopResponse(BaseModel):
    drift: DriftPipelineResponse
    diagnoses: list[LoopDiagnosisItem]


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.get("/quality/trend", response_model=QualityTrendResponse)
def get_quality_trend(
    days: int = Query(default=14, ge=1, le=90),
    model: str | None = Query(default=None),
    db: Session = Depends(get_db_session),
) -> QualityTrendResponse:
    """Daily quality score trend from conversation_events."""
    params: dict[str, Any] = {"days": days}
    model_filter = ""
    if model:
        model_filter = "AND llm_model_used = :model"
        params["model"] = model

    try:
        rows = db.execute(text(f"""
            SELECT DATE(created_at) AS d,
                   AVG(quality_score) AS avg_score,
                   COUNT(*) AS cnt,
                   llm_model_used
            FROM conversation_events
            WHERE quality_score IS NOT NULL
              AND created_at >= DATE_SUB(NOW(), INTERVAL :days DAY)
              {model_filter}
            GROUP BY d, llm_model_used
            ORDER BY d ASC
        """), params).fetchall()
    except Exception:
        logger.debug("quality/trend: table not ready, returning empty")
        return QualityTrendResponse(points=[], overall_avg=0.0, total_events=0)

    points = [
        QualityTrendPoint(
            date=str(r[0]), avg_score=round(float(r[1]), 2),
            count=int(r[2]), model=r[3],
        )
        for r in rows
    ]
    total = sum(p.count for p in points)
    overall = (
        sum(p.avg_score * p.count for p in points) / total
        if total else 0.0
    )
    return QualityTrendResponse(
        points=points, overall_avg=round(overall, 2), total_events=total,
    )


@router.get("/drift", response_model=list[DriftSignalResponse])
def detect_drift(
    db: Session = Depends(get_db_session),
) -> list[DriftSignalResponse]:
    """Run drift detection and return active signals."""
    from core.evaluation.drift_detector import DriftDetector

    try:
        signals = DriftDetector(db).detect()
    except Exception:
        logger.debug("drift: detector not ready, returning empty")
        return []
    return [
        DriftSignalResponse(
            model=s.model, template_id=s.template_id,
            current_avg=round(s.current_avg, 2),
            previous_avg=round(s.previous_avg, 2),
            delta=round(s.week_delta, 2),
            severity=s.severity.value,
            sample_count=s.sample_count,
        )
        for s in signals
    ]


@router.get("/gates", response_model=list[GateResultResponse])
def get_gate_history(
    limit: int = Query(default=20, ge=1, le=100),
    db: Session = Depends(get_db_session),
) -> list[GateResultResponse]:
    """Recent regression gate results."""
    try:
        rows = db.execute(text("""
            SELECT gate_id, change_type, change_id, sessions_tested,
                   error_rate, score_delta, passed, created_at
            FROM gate_results
            ORDER BY created_at DESC
            LIMIT :limit
        """), {"limit": limit}).fetchall()
    except Exception:
        return []

    return [
        GateResultResponse(
            gate_id=r[0], change_type=r[1], change_id=r[2],
            sessions_tested=int(r[3]), error_rate=float(r[4]),
            score_delta=float(r[5]), passed=bool(r[6]),
            created_at=r[7].isoformat() if r[7] else None,
        )
        for r in rows
    ]


@router.get("/calibration", response_model=CalibrationResponse)
def get_calibration(
    agent_id: str | None = Query(default=None),
    days: int = Query(default=30, ge=1, le=90),
    db: Session = Depends(get_db_session),
) -> CalibrationResponse:
    """Confidence calibration status — how well the system knows what it doesn't know."""
    from core.evaluation.confidence_calibrator import ConfidenceCalibrator

    try:
        cal = ConfidenceCalibrator(db)
        result = cal.measure(agent_id=agent_id, days=days)
        adj = cal.compute_adjustment(result)
    except Exception:
        logger.debug("calibration: not ready, returning defaults")
        return CalibrationResponse(
            mean_confidence=0.0, mean_quality=0.0, calibration_error=0.0,
            bias=0.0, sample_count=0, adjustment_multiplier=1.0,
            adjustment_reason="no data",
        )
    return CalibrationResponse(
        mean_confidence=result.mean_confidence,
        mean_quality=result.mean_quality,
        calibration_error=result.calibration_error,
        bias=result.bias,
        sample_count=result.sample_count,
        adjustment_multiplier=adj["multiplier"],
        adjustment_reason=adj["reason"],
    )


@router.get("/sessions/scores", response_model=list[SessionScoreResponse])
def get_session_scores(
    limit: int = Query(default=20, ge=1, le=100),
    min_score: float = Query(default=0.0, ge=0.0, le=5.0),
    db: Session = Depends(get_db_session),
) -> list[SessionScoreResponse]:
    """Session-level quality scores from quality_assessments."""
    try:
        rows = db.execute(text("""
            SELECT target_id, score, COALESCE(step_count, 0)
            FROM quality_assessments
            WHERE level = 'session' AND score >= :min_score
            ORDER BY updated_at DESC
            LIMIT :limit
        """), {"limit": limit, "min_score": min_score}).fetchall()
    except Exception:
        logger.debug("sessions/scores: table not ready, returning empty")
        return []

    return [
        SessionScoreResponse(
            session_id=r[0], score=round(float(r[1]), 2), chain_count=int(r[2]),
        )
        for r in rows
    ]


# ---------------------------------------------------------------------------
# Action endpoints — closed-loop evaluation
# ---------------------------------------------------------------------------


@router.post("/gate/validate", response_model=GateValidateResponse)
def validate_gate(
    req: GateValidateRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
) -> GateValidateResponse:
    """Trigger regression gate: replay golden sessions against a proposed change."""
    from core.evaluation.regression_gate import ChangeType, RegressionGate

    try:
        gate = RegressionGate(db=db)
        result = gate.validate_change(
            change_type=ChangeType(req.change_type),
            change_id=req.change_id,
            change_content=req.change_content,
            golden_session_count=req.golden_session_count,
            error_rate_threshold=req.error_rate_threshold,
            score_regression_threshold=req.score_regression_threshold,
        )
    except Exception as e:
        logger.error("Gate validation failed: %s", e)
        return GateValidateResponse(
            gate_id="", verdict="error", reason=str(e),
            sessions_tested=0, metrics={},
        )
    return GateValidateResponse(
        gate_id=result["gate_id"],
        verdict=result["verdict"],
        reason=result["reason"],
        sessions_tested=result["sessions_tested"],
        metrics=result.get("metrics", {}),
    )


@router.post("/drift/run", response_model=DriftPipelineResponse)
def run_drift(
    current_user: dict = Depends(get_current_user),
) -> DriftPipelineResponse:
    """Run full drift pipeline: detect → confirm → correct."""
    from core.evaluation.drift_pipeline import run_drift_pipeline

    try:
        result = run_drift_pipeline(db_factory=SessionLocal)
    except Exception as e:
        logger.error("Drift pipeline failed: %s", e)
        return DriftPipelineResponse(
            signals_detected=0, signals_confirmed=0,
            corrections_applied=0, actions=[], error=str(e),
        )
    return DriftPipelineResponse(
        signals_detected=result.signals_detected,
        signals_confirmed=result.signals_confirmed,
        corrections_applied=result.corrections_applied,
        actions=result.actions,
        error=result.error,
    )


@router.post("/loop", response_model=ClosedLoopResponse)
def run_closed_loop(
    days: int = Query(default=7, ge=1, le=30),
    dry_run: bool = Query(default=False),
    current_user: dict = Depends(get_current_user),
) -> ClosedLoopResponse:
    """One-click closed loop: drift pipeline + InputFaceLearner diagnose-and-fix.

    This is the OBSERVE → DIAGNOSE → PROPOSE → VALIDATE → DEPLOY loop
    from the design doc, exposed as a single API call.
    """
    from core.evaluation.drift_pipeline import run_drift_pipeline

    # Phase 1: Drift detection + auto-correction
    try:
        drift_result = run_drift_pipeline(db_factory=SessionLocal)
    except Exception as e:
        logger.error("Closed loop drift phase failed: %s", e)
        drift_result = None

    drift_resp = DriftPipelineResponse(
        signals_detected=drift_result.signals_detected if drift_result else 0,
        signals_confirmed=drift_result.signals_confirmed if drift_result else 0,
        corrections_applied=drift_result.corrections_applied if drift_result else 0,
        actions=drift_result.actions if drift_result else [],
        error=drift_result.error if drift_result else None,
    )

    # Phase 2: InputFaceLearner — diagnose bottlenecks + propose/apply fixes
    diagnoses: list[LoopDiagnosisItem] = []
    try:
        from core.learning.input_face_learner import InputFaceLearner
        from core.llm.client import LLMClient

        db = SessionLocal()
        try:
            llm = LLMClient(db)
            learner = InputFaceLearner(db, llm)
            results = learner.diagnose_and_fix(days=days, dry_run=dry_run)
            diagnoses = [
                LoopDiagnosisItem(
                    input_face=r.input_face.value,
                    bottleneck=r.bottleneck,
                    applied=r.applied,
                    gate_verdict=r.gate_verdict,
                    error=r.error,
                )
                for r in results
            ]
        finally:
            db.close()
    except Exception as e:
        logger.error("Closed loop learner phase failed: %s", e)
        diagnoses = [LoopDiagnosisItem(
            input_face="all", bottleneck="learner_unavailable",
            applied=False, gate_verdict="error", error=str(e),
        )]

    return ClosedLoopResponse(drift=drift_resp, diagnoses=diagnoses)
