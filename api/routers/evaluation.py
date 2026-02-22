"""Evaluation API — quality trends, drift detection, gate history, calibration."""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Depends, Query
from pydantic import BaseModel
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session
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

    signals = DriftDetector(db).detect()
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

    cal = ConfidenceCalibrator(db)
    result = cal.measure(agent_id=agent_id, days=days)
    adj = cal.compute_adjustment(result)
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
    rows = db.execute(text("""
        SELECT target_id, score, COALESCE(step_count, 0)
        FROM quality_assessments
        WHERE level = 'session' AND score >= :min_score
        ORDER BY updated_at DESC
        LIMIT :limit
    """), {"limit": limit, "min_score": min_score}).fetchall()

    return [
        SessionScoreResponse(
            session_id=r[0], score=round(float(r[1]), 2), chain_count=int(r[2]),
        )
        for r in rows
    ]
