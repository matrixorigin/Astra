"""Evaluation API — quality trends, drift detection, gate history, calibration, closed-loop actions."""

from __future__ import annotations

import json
from typing import Any

from fastapi import APIRouter, Depends, Query
from pydantic import BaseModel, Field
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import SessionLocal
from api.dependencies import get_current_user
from core.logging_config import get_logger
from core.utils.id_generator import generate_id

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
    loop_id: str
    drift: DriftPipelineResponse
    calibration: CalibrationResponse | None = None
    diagnoses: list[LoopDiagnosisItem]
    skill_learning: dict[str, Any] | None = None


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.get("/quality/trend", response_model=QualityTrendResponse)
def get_quality_trend(
    days: int = Query(default=14, ge=1, le=90),
    model: str | None = Query(default=None),
    current_user: dict = Depends(get_current_user),
) -> QualityTrendResponse:
    """Daily quality score trend from conversation_events."""
    db = SessionLocal()
    try:
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

    finally:
        db.close()

@router.get("/drift", response_model=list[DriftSignalResponse])
def detect_drift(
    current_user: dict = Depends(get_current_user),
) -> list[DriftSignalResponse]:
    """Run drift detection and return active signals."""
    from core.evaluation.drift_detector import DriftDetector

    try:
        signals = DriftDetector(SessionLocal).detect()
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
    current_user: dict = Depends(get_current_user),
) -> list[GateResultResponse]:
    """Recent regression gate results."""
    db = SessionLocal()
    try:
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

    finally:
        db.close()

@router.get("/calibration", response_model=CalibrationResponse)
def get_calibration(
    agent_id: str | None = Query(default=None),
    days: int = Query(default=30, ge=1, le=90),
    current_user: dict = Depends(get_current_user),
) -> CalibrationResponse:
    """Confidence calibration status — how well the system knows what it doesn't know."""
    from core.evaluation.confidence_calibrator import ConfidenceCalibrator

    try:
        cal = ConfidenceCalibrator(SessionLocal)
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
    current_user: dict = Depends(get_current_user),
) -> list[SessionScoreResponse]:
    """Session-level quality scores from quality_assessments."""
    db = SessionLocal()
    try:
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

    finally:
        db.close()

# ---------------------------------------------------------------------------
# Action endpoints — closed-loop evaluation
# ---------------------------------------------------------------------------


@router.post("/gate/validate", response_model=GateValidateResponse)
def validate_gate(
    req: GateValidateRequest,
    current_user: dict = Depends(get_current_user),
) -> GateValidateResponse:
    """Trigger regression gate: replay golden sessions against a proposed change."""
    from core.evaluation.regression_gate import ChangeType, RegressionGate

    gate = RegressionGate(db_factory=SessionLocal)
    result = gate.validate_change(
        change_type=ChangeType(req.change_type),
        change_id=req.change_id,
        change_content=req.change_content,
        golden_session_count=req.golden_session_count,
        error_rate_threshold=req.error_rate_threshold,
        score_regression_threshold=req.score_regression_threshold,
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

    result = run_drift_pipeline(db_factory=SessionLocal)
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
    """Full closed loop: OBSERVE → DIAGNOSE → PROPOSE → VALIDATE → DEPLOY → RECORD.

    Phase 1 — Drift: detect quality drift, confirm via replay, auto-correct.
    Phase 2 — Calibration: measure confidence calibration, compute adjustment.
    Phase 3 — Learner: diagnose input-face bottlenecks, propose + validate + deploy fixes.
              If drift found template-level issues, learner targets PROMPT face specifically.
    Phase 4 — Skill selection: learn from recent skill selection failures.
    Record — Persist loop execution as auditable event.
    """
    from core.evaluation.confidence_calibrator import ConfidenceCalibrator
    from core.evaluation.drift_pipeline import PipelineResult, run_drift_pipeline
    from core.learning.input_face_learner import InputFace, InputFaceLearner

    loop_id = generate_id()

    # Phase 1: Drift detection + auto-correction
    try:
        drift_result = run_drift_pipeline(db_factory=SessionLocal)
    except Exception as e:
        logger.error("Closed loop drift phase failed: %s", e)
        drift_result = PipelineResult(error=str(e))

    drift_resp = DriftPipelineResponse(
        signals_detected=drift_result.signals_detected,
        signals_confirmed=drift_result.signals_confirmed,
        corrections_applied=drift_result.corrections_applied,
        actions=drift_result.actions,
        error=drift_result.error,
    )

    # Phase 2: Calibration
    calibration_resp: CalibrationResponse | None = None
    db = SessionLocal()
    try:
        cal = ConfidenceCalibrator(SessionLocal)
        cal_result = cal.measure(days=days)
        adj = cal.compute_adjustment(cal_result)
        calibration_resp = CalibrationResponse(
            mean_confidence=cal_result.mean_confidence,
            mean_quality=cal_result.mean_quality,
            calibration_error=cal_result.calibration_error,
            bias=cal_result.bias,
            sample_count=cal_result.sample_count,
            adjustment_multiplier=adj["multiplier"],
            adjustment_reason=adj["reason"],
        )
    except Exception as e:
        logger.error("Closed loop calibration phase failed: %s", e)
    finally:
        db.close()

    # Phase 3: InputFaceLearner — drift-informed targeted diagnosis
    # If drift found template-level issues, focus learner on PROMPT face
    faces: list[InputFace] | None = None
    if any(a.get("template_id") for a in drift_result.actions):
        faces = [InputFace.PROMPT]

    diagnoses: list[LoopDiagnosisItem] = []
    db = SessionLocal()
    try:
        from core.llm.client import LLMClient

        llm = LLMClient(SessionLocal)
        learner = InputFaceLearner(SessionLocal, llm)
        results = learner.diagnose_and_fix(days=days, dry_run=dry_run, faces=faces)
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
    except Exception as e:
        logger.error("Closed loop learner phase failed: %s", e)
        diagnoses = [LoopDiagnosisItem(
            input_face="all", bottleneck="learner_unavailable",
            applied=False, gate_verdict="error", error=str(e),
        )]
    finally:
        db.close()

    # Phase 4: Skill selection learning
    # Separate from InputFaceLearner — SelfImprovingSelector has its own
    # signal extraction, multi-factor scoring, and sandbox-based validation
    skill_learning_resp: dict[str, Any] | None = None
    db = SessionLocal()
    try:
        from core.skills.self_improving_selector import SelfImprovingSelector

        selector = SelfImprovingSelector(SessionLocal)
        skill_learning_resp = selector.learn_from_failures(days=days)
    except Exception as e:
        logger.error("Closed loop skill learning phase failed: %s", e)
        skill_learning_resp = {"learned": 0, "error": str(e)}
    finally:
        db.close()

    # Record — audit trail for the loop execution itself
    _record_loop_event(
        loop_id, drift_resp, calibration_resp, diagnoses,
        skill_learning_resp, dry_run,
    )

    return ClosedLoopResponse(
        loop_id=loop_id,
        drift=drift_resp,
        calibration=calibration_resp,
        diagnoses=diagnoses,
        skill_learning=skill_learning_resp,
    )


def _record_loop_event(
    loop_id: str,
    drift: DriftPipelineResponse,
    calibration: CalibrationResponse | None,
    diagnoses: list[LoopDiagnosisItem],
    skill_learning: dict[str, Any] | None,
    dry_run: bool,
) -> None:
    """Persist closed-loop execution as an auditable conversation event."""
    db = SessionLocal()
    try:
        # causal_chain_id = loop_id: each loop execution is its own causal chain root
        db.execute(text("""
            INSERT INTO conversation_events
            (event_id, session_id, user_id, agent_id, agent_version,
             event_type, content, causal_chain_id, created_at)
            VALUES (:eid, 'system', 'system', 'system', '1.0.0',
                    'closed_loop_execution', :content, :eid, NOW())
        """), {
            "eid": loop_id,
            "content": json.dumps({
                "dry_run": dry_run,
                "drift": {"detected": drift.signals_detected, "confirmed": drift.signals_confirmed,
                          "corrected": drift.corrections_applied},
                "calibration": {"error": calibration.calibration_error,
                                "bias": calibration.bias} if calibration else None,
                "diagnoses": [{"face": d.input_face, "applied": d.applied,
                               "verdict": d.gate_verdict} for d in diagnoses],
                "skill_learning": skill_learning,
            }),
        })
        db.commit()
    except Exception as e:
        logger.warning("Failed to record loop event: %s", e)
        db.rollback()
    finally:
        db.close()


# ---------------------------------------------------------------------------
# Trust Report — aggregated trust health indicators
# ---------------------------------------------------------------------------

class TrustReportResponse(BaseModel):
    """Aggregated trust health across confidence, SLO, drift, hallucination."""
    confidence_calibration: dict[str, Any] | None = None
    slo_summary: dict[str, Any] | None = None
    drift_summary: dict[str, Any] | None = None
    hallucination_stats: dict[str, Any] | None = None
    overall_trust_score: float = 0.0


@router.get("/trust-report", response_model=TrustReportResponse)
def trust_report(
    agent_id: str = Query(default="dev-agent"),
    days: int = Query(default=7, ge=1, le=90),
    current_user: dict = Depends(get_current_user),
) -> TrustReportResponse:
    """Aggregated trust health report: confidence, SLO, drift, hallucination."""
    scores: list[float] = []
    result = TrustReportResponse()

    # Phases 1-3 use DbConsumer components that manage their own sessions.
    # Phase 4 (hallucination stats) needs a direct SQL query.

    # 1. Confidence calibration
    try:
        from core.evaluation.confidence_calibrator import ConfidenceCalibrator
        cal = ConfidenceCalibrator(SessionLocal)
        cal_result = cal.measure(agent_id=agent_id, days=days)
        result.confidence_calibration = {
            "calibration_error": round(cal_result.calibration_error, 4),
            "bias": round(cal_result.bias, 4),
            "sample_count": cal_result.sample_count,
        }
        scores.append(max(0, 1.0 - cal_result.calibration_error))
    except Exception as e:
        logger.debug("Trust report calibration skipped: %s", e)

    # 2. SLO compliance
    try:
        from core.evaluation.slo_monitor import SLOMonitor
        monitor = SLOMonitor(SessionLocal)
        report = monitor.check_agent(agent_id, period_days=days)
        total = len(report.statuses)
        met = sum(1 for s in report.statuses if s.met)
        result.slo_summary = {
            "total_slos": total,
            "met": met,
            "violated": total - met,
            "compliance_rate": round(met / total, 4) if total else 1.0,
        }
        scores.append(met / total if total else 1.0)
    except Exception as e:
        logger.debug("Trust report SLO skipped: %s", e)

    # 3. Drift
    try:
        from core.evaluation.drift_detector import DriftDetector
        detector = DriftDetector(SessionLocal)
        signals = detector.detect()
        critical = sum(1 for s in signals if s.severity.value == "critical")
        result.drift_summary = {
            "total_signals": len(signals),
            "critical": critical,
            "warning": sum(1 for s in signals if s.severity.value == "warning"),
        }
        scores.append(1.0 if critical == 0 else max(0, 1.0 - critical * 0.2))
    except Exception as e:
        logger.debug("Trust report drift skipped: %s", e)

    # 4. Hallucination stats — direct SQL, needs its own short-lived session
    db = SessionLocal()
    try:
        row = db.execute(text("""
            SELECT COUNT(*) as total,
                   SUM(CASE WHEN JSON_UNQUOTE(JSON_EXTRACT(content, '$.safe_to_deliver')) = 'true' THEN 1 ELSE 0 END) as safe
            FROM conversation_events
            WHERE event_type = 'hallucination_check'
              AND created_at > DATE_SUB(NOW(), INTERVAL :days DAY)
        """), {"days": days}).fetchone()
        if row and row[0] > 0:
            result.hallucination_stats = {
                "checks_total": row[0],
                "safe_deliveries": row[1] or 0,
                "safety_rate": round((row[1] or 0) / row[0], 4),
            }
            scores.append((row[1] or 0) / row[0])
    except Exception as e:
        logger.debug("Trust report hallucination skipped: %s", e)
    finally:
        db.close()

    result.overall_trust_score = round(sum(scores) / len(scores), 4) if scores else 0.0
    return result

# ---------------------------------------------------------------------------
# SLO Dashboard — per-agent SLO status + history + auto-response
# ---------------------------------------------------------------------------

class SLODashboardEntry(BaseModel):
    agent_id: str
    statuses: list[dict[str, Any]]
    period_days: int


class SLODashboardResponse(BaseModel):
    agents: list[SLODashboardEntry]


@router.get("/slo/dashboard", response_model=SLODashboardResponse)
def slo_dashboard(
    period_days: int = Query(default=30, ge=1, le=90),
    current_user: dict = Depends(get_current_user),
):
    """SLO dashboard: check all agents and return compliance status."""
    db = SessionLocal()
    try:
        try:
            rows = db.execute(text("""
                SELECT DISTINCT agent_id FROM conversation_events
                WHERE event_type = 'llm_response'
                  AND created_at > DATE_SUB(NOW(), INTERVAL :days DAY)
                  AND agent_id IS NOT NULL
            """), {"days": period_days}).fetchall()
            agent_ids = [r[0] for r in rows] if rows else []

            from core.evaluation.slo_monitor import SLOMonitor
            monitor = SLOMonitor(SessionLocal)
            entries = []
            for aid in agent_ids:
                report = monitor.check_agent(aid, period_days=period_days)
                entries.append(SLODashboardEntry(
                    agent_id=aid,
                    statuses=[
                        {
                            "slo": s.slo.name,
                            "target": s.slo.target,
                            "current": s.current_value,
                            "met": s.met,
                            "burn_rate": s.burn_rate,
                            "severity": s.severity.value,
                            "bad_days": s.bad_days,
                        }
                        for s in report.statuses
                    ],
                    period_days=period_days,
                ))
            return SLODashboardResponse(agents=entries)
        except Exception as e:
            raise HTTPException(status_code=500, detail=str(e))

    finally:
        db.close()

@router.get("/slo/{agent_id}/history")
def slo_history(
    agent_id: str,
    days: int = Query(default=30, ge=1, le=90),
    current_user: dict = Depends(get_current_user),
):
    """SLO history: daily metrics for a single agent."""
    from core.evaluation.slo_monitor import SLOMonitor
    monitor = SLOMonitor(SessionLocal)
    metrics = monitor.get_daily_metrics(agent_id, days)
    return {"agent_id": agent_id, "days": days, "daily_metrics": metrics}


# ---------------------------------------------------------------------------
# Observability Metrics — 6-layer aggregation
# ---------------------------------------------------------------------------

@router.get("/observability/metrics")
def observability_metrics(
    agent_id: str = Query(default="dev-agent"),
    days: int = Query(default=7, ge=1, le=90),
    current_user: dict = Depends(get_current_user),
):
    """Aggregated observability metrics across 6 layers (trust-and-safety.md §5)."""
    db = SessionLocal()
    try:
        result: dict[str, Any] = {}

        # Decision layer
        row = db.execute(text("""
            SELECT AVG(quality_score) as avg_quality,
                   COUNT(*) as total_responses
            FROM conversation_events
            WHERE agent_id = :aid AND event_type = 'llm_response'
              AND created_at > DATE_SUB(NOW(), INTERVAL :days DAY)
        """), {"aid": agent_id, "days": days}).fetchone()
        result["decision"] = {
            "avg_quality": round(float(row[0]), 4) if row and row[0] else 0,
            "total_responses": int(row[1]) if row else 0,
        }

        # Session layer
        row = db.execute(text("""
            SELECT COUNT(DISTINCT session_id) as sessions,
                   AVG(turn_count) as avg_turns
            FROM (
                SELECT session_id, COUNT(*) as turn_count
                FROM conversation_events
                WHERE agent_id = :aid
                  AND created_at > DATE_SUB(NOW(), INTERVAL :days DAY)
                GROUP BY session_id
            ) sub
        """), {"aid": agent_id, "days": days}).fetchone()
        result["session"] = {
            "active_sessions": int(row[0]) if row and row[0] else 0,
            "avg_turns_per_session": round(float(row[1]), 1) if row and row[1] else 0,
        }

        # Skill layer
        row = db.execute(text("""
            SELECT COUNT(*) as total,
                   SUM(CASE WHEN execution_success = 1 THEN 1 ELSE 0 END) as ok
            FROM skill_selection_events
            WHERE created_at > DATE_SUB(NOW(), INTERVAL :days DAY)
        """), {"days": days}).fetchone()
        total_sel = int(row[0]) if row and row[0] else 0
        ok_sel = int(row[1]) if row and row[1] else 0
        result["skill"] = {
            "total_selections": total_sel,
            "success_rate": round(ok_sel / total_sel, 4) if total_sel else 0,
        }

        return {"agent_id": agent_id, "period_days": days, "metrics": result}

    finally:
        db.close()

# ---------------------------------------------------------------------------
# Memory Health — aggregated memory pipeline status
# ---------------------------------------------------------------------------

class MemoryHealthResponse(BaseModel):
    observations: dict[str, int] = {}
    reflections: dict[str, int] = {}
    knowledge: dict[str, int] = {}
    pollution: dict[str, int] = {}
    governance: dict[str, Any] = {}


@router.get("/memory-health", response_model=MemoryHealthResponse)
def memory_health(
    user_id: str | None = Query(default=None),
    current_user: dict = Depends(get_current_user),
) -> MemoryHealthResponse:
    """Memory pipeline health: observations, reflections, knowledge, pollution."""
    uid = user_id or current_user.get("user_id", "system")
    result = MemoryHealthResponse()
    db = SessionLocal()
    try:
        # Observations
        try:
            row = db.execute(text("""
                SELECT COUNT(*) as total,
                       SUM(CASE WHEN is_reflected = 0 THEN 1 ELSE 0 END) as pending
                FROM observations WHERE user_id = :uid
            """), {"uid": uid}).fetchone()
            if row:
                result.observations = {"total": row[0], "pending_reflection": row[1] or 0}
        except Exception:
            pass

        # Knowledge entries
        try:
            row = db.execute(text("""
                SELECT COUNT(*) as total,
                       SUM(CASE WHEN confidence < 0.3 THEN 1 ELSE 0 END) as low_conf,
                       SUM(CASE WHEN confidence = 0 THEN 1 ELSE 0 END) as quarantined
                FROM sk_knowledge_entries WHERE user_id = :uid
            """), {"uid": uid}).fetchone()
            if row:
                result.knowledge = {
                    "total": row[0],
                    "low_confidence": row[1] or 0,
                    "quarantined": row[2] or 0,
                }
        except Exception:
            pass

        # Recent governance runs
        try:
            rows = db.execute(text("""
                SELECT task_name, result
                FROM governance_runs
                ORDER BY completed_at DESC LIMIT 5
            """)).fetchall()
            if rows:
                result.governance = {r[0]: r[1] for r in rows if r[1]}
        except Exception:
            pass
    finally:
        db.close()

    return result

# ── Training Data Pipeline ─────────────────────────────────────────────────────

class TrainingDataExtractRequest(BaseModel):
    name: str = "auto_extract"
    description: str = ""
    quality_threshold: float = 0.75
    sample_size: int | None = None


class TrainingDatasetResponse(BaseModel):
    dataset_id: str
    example_count: int
    avg_quality: float | None = None


@router.post("/training-data/extract", response_model=TrainingDatasetResponse)
def extract_training_data(
    req: TrainingDataExtractRequest,
    _user: dict = Depends(get_current_user),
):
    """Extract high-quality conversation pairs as training data."""
    from core.data_versioning.training_data_pipeline import DatasetConfig, TrainingDataPipeline
    from core.utils.id_generator import generate_id
    pipeline = TrainingDataPipeline(SessionLocal)
    dataset_id = generate_id()
    config = DatasetConfig(
        dataset_id=dataset_id,
        name=req.name,
        description=req.description,
        quality_threshold=req.quality_threshold,
        sample_size=req.sample_size,
    )
    pipeline.create_dataset(config)
    examples = pipeline.extract_examples(dataset_id, quality_threshold=req.quality_threshold)
    avg_q = sum(e.quality_score for e in examples) / len(examples) if examples else None
    return TrainingDatasetResponse(
        dataset_id=dataset_id,
        example_count=len(examples),
        avg_quality=round(avg_q, 2) if avg_q else None,
    )


@router.get("/training-data/{dataset_id}/export")
def export_training_data(
    dataset_id: str,
    format: str = "jsonl",
    _user: dict = Depends(get_current_user),
):
    """Export a training dataset as JSONL file."""
    from core.data_versioning.training_data_pipeline import TrainingDataPipeline
    pipeline = TrainingDataPipeline(SessionLocal)
    try:
        output_path = pipeline.export_dataset(dataset_id, format=format)
    except Exception as e:
        raise HTTPException(status_code=404, detail=str(e))
    from fastapi.responses import FileResponse
    return FileResponse(path=output_path, media_type="application/jsonl", filename=f"{dataset_id}.{format}")
