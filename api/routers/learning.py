"""Learning service API endpoints."""

from datetime import datetime, timezone
from typing import Any

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, Field

from api.database import SessionLocal
from api.dependencies import get_current_user
from core.llm.client import LLMClient
from core.logging_config import get_logger
# Removed: learning_signals and pipeline modules deleted

logger = get_logger(__name__)

router = APIRouter(prefix="/api/v1/learning", tags=["learning"])


# Request/Response models
class LearningTriggerRequest(BaseModel):
    """Trigger learning cycle request."""
    days: int = Field(default=7, ge=1, le=30, description="Look back N days")
    force: bool = Field(default=False, description="Force learning even in cooldown")
    signal_types: list[str] = Field(
        default=["wrong_skill"],
        description="Learning signal types to process"
    )
    weights: dict[str, float] | None = Field(
        default=None,
        description="Custom weights for multi-dimensional scoring"
    )


class LearningTriggerResponse(BaseModel):
    """Learning cycle response."""
    status: str
    learned: int
    signals_by_type: dict[str, int] | None = None
    gate_verdict: str | None = None
    improvement_pct: float | None = None
    test_count: int | None = None
    error: str | None = None
    message: str | None = None
    model_version: str = "v1.0"


class SkillStatsEntry(BaseModel):
    """Per-skill execution statistics."""
    selection_count: int
    success_rate: float
    avg_cost_usd: float
    avg_time_ms: float


class LearningStatsResponse(BaseModel):
    """Learning statistics response."""
    total_learnings: int
    high_confidence: int
    low_confidence: int
    avg_confidence: float
    by_signal_type: dict[str, int]
    weights: dict[str, float]
    weights_per_signal: dict[str, dict[str, float]]
    decay: dict[str, Any]
    total_gates: int
    passed_gates: int
    failed_gates: int
    pass_rate: float
    avg_improvement_pct: float
    per_skill: dict[str, SkillStatsEntry] = {}
    last_learning_time: datetime | None = None


class SignalTypesResponse(BaseModel):
    """Available signal types response."""
    signal_types: list[str]
    descriptions: dict[str, str]


class FeedbackRequest(BaseModel):
    """Submit feedback request."""
    event_id: str
    feedback_type: str = Field(
        description="Signal type: wrong_skill | slow_execution | high_cost | low_satisfaction"
    )
    correct_skills: list[str] | None = None
    satisfaction_score: int | None = Field(default=None, ge=1, le=5)
    comment: str | None = None


class FeedbackResponse(BaseModel):
    """Feedback submission response."""
    status: str
    message: str


# Endpoints
@router.post("/trigger", response_model=LearningTriggerResponse)
async def trigger_learning(
    request: LearningTriggerRequest,
    current_user: dict = Depends(get_current_user),
) -> LearningTriggerResponse:
    """Trigger learning cycle — disabled after skill pipeline removal."""
    return LearningTriggerResponse(
        status="error",
        learned=0,
        error="Learning pipeline removed in skill system cleanup",
    )


@router.get("/signals", response_model=SignalTypesResponse)
async def get_signal_types(
    current_user: dict = Depends(get_current_user),
) -> SignalTypesResponse:
    """Get available learning signal types — stub after pipeline removal."""
    return SignalTypesResponse(
        signal_types=["wrong_skill", "slow_execution", "high_cost", "low_satisfaction"],
        descriptions={
            "wrong_skill": "Incorrect skill selection",
            "slow_execution": "Execution time exceeds threshold",
            "high_cost": "Execution cost exceeds budget",
            "low_satisfaction": "User satisfaction below threshold",
        }
    )


@router.get("/stats", response_model=LearningStatsResponse)
async def get_learning_stats(
    current_user: dict = Depends(get_current_user),
) -> LearningStatsResponse:
    """Get learning statistics — stub after pipeline removal."""
    return LearningStatsResponse(
        total_learnings=0, high_confidence=0, low_confidence=0,
        avg_confidence=0.0, by_signal_type={},
        weights={}, weights_per_signal={}, decay={},
        total_gates=0, passed_gates=0, failed_gates=0,
        pass_rate=0.0, avg_improvement_pct=0.0,
    )


@router.post("/feedback", response_model=FeedbackResponse)
async def submit_feedback(
    request: FeedbackRequest,
    current_user: dict = Depends(get_current_user),
) -> FeedbackResponse:
    """Submit feedback for a skill selection event.
    
    Feedback helps the system learn from:
    - Wrong skill selections
    - Slow executions
    - High costs
    - Low user satisfaction
    
    Args:
        request: Feedback data
        db: Database session
        
    Returns:
        Feedback submission result
    """
    try:
        from api.models import SkillSelectionEvent

        db = SessionLocal()
        try:
            # Find event
            event = db.query(SkillSelectionEvent).filter(
                SkillSelectionEvent.event_id == request.event_id
            ).first()

            if not event:
                raise HTTPException(status_code=404, detail="Event not found")

            # Update event based on feedback type
            if request.feedback_type == "wrong_skill":
                event.selection_correctness = 0
                event.correction_suggestion = request.correct_skills

            if request.satisfaction_score:
                event.user_feedback_score = request.satisfaction_score
                event.selection_correctness = 1 if request.satisfaction_score >= 4 else 0

            db.commit()
        finally:
            db.close()

        return FeedbackResponse(
            status="success",
            message=f"Feedback recorded for event {request.event_id}"
        )

    except HTTPException:
        raise
    except Exception as e:
        logger.error(f"Submit feedback failed: {e}")
        raise HTTPException(status_code=500, detail=str(e))


@router.get("/health")
async def health_check() -> dict[str, Any]:
    """Health check endpoint."""
    return {
        "status": "healthy",
        "service": "learning",
        "version": "1.0.0",
        "timestamp": datetime.now(timezone.utc).isoformat(),
    }
