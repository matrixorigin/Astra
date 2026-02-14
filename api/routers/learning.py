"""Learning service API endpoints."""

from datetime import datetime
from typing import Any

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from api.database import get_db_session
from core.agent.selector import AgentSkillSelector
from core.llm.client import LLMClient
from core.logging_config import get_logger
from uuid_utils import uuid7

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


class LearningTriggerResponse(BaseModel):
    """Learning cycle response."""
    status: str
    learned: int
    gate_verdict: str | None = None
    improvement_pct: float | None = None
    test_count: int | None = None
    error: str | None = None
    message: str | None = None
    model_version: str = "v1.0"


class LearningStatsResponse(BaseModel):
    """Learning statistics response."""
    total_learnings: int
    high_confidence: int
    low_confidence: int
    avg_confidence: float
    total_gates: int
    passed_gates: int
    failed_gates: int
    pass_rate: float
    avg_improvement_pct: float
    last_learning_time: datetime | None = None


class FeedbackRequest(BaseModel):
    """Submit feedback request."""
    event_id: str
    feedback_type: str = Field(description="wrong_skill | slow_execution | high_cost")
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
    db: Session = Depends(get_db_session),
) -> LearningTriggerResponse:
    """Trigger learning cycle from recent failures.
    
    This endpoint initiates a learning cycle that:
    1. Analyzes recent selection failures
    2. Extracts learning patterns
    3. Validates through regression gate
    4. Deploys if gate passes
    
    Args:
        request: Learning trigger parameters
        db: Database session
        
    Returns:
        Learning cycle results
    """
    try:
        llm_client = LLMClient(db)
        selector = AgentSkillSelector(
            db=db,
            llm_client=llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        # Trigger learning
        result = selector.learn_from_failures(
            days=request.days,
            force=request.force,
        )
        
        # Handle errors
        if result.get("error"):
            return LearningTriggerResponse(
                status="error",
                learned=result.get("learned", 0),
                error=result["error"],
                message=result.get("message"),
            )
        
        # Success
        return LearningTriggerResponse(
            status="success",
            learned=result["learned"],
            gate_verdict=result.get("gate_verdict"),
            improvement_pct=result.get("improvement_pct"),
            test_count=result.get("test_count"),
        )
        
    except Exception as e:
        logger.error(f"Learning trigger failed: {e}")
        raise HTTPException(status_code=500, detail=str(e))


@router.get("/stats", response_model=LearningStatsResponse)
async def get_learning_stats(
    db: Session = Depends(get_db_session),
) -> LearningStatsResponse:
    """Get learning statistics.
    
    Returns comprehensive statistics about:
    - Total learnings and confidence distribution
    - Regression gate results
    - Learning effectiveness
    
    Args:
        db: Database session
        
    Returns:
        Learning statistics
    """
    try:
        llm_client = LLMClient(db)
        selector = AgentSkillSelector(
            db=db,
            llm_client=llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        stats = selector.get_learning_stats()
        
        return LearningStatsResponse(
            total_learnings=stats["learnings"]["total_learnings"],
            high_confidence=stats["learnings"]["high_confidence"],
            low_confidence=stats["learnings"]["low_confidence"],
            avg_confidence=stats["learnings"]["avg_confidence"],
            total_gates=stats["regression_gates"]["total_gates"],
            passed_gates=stats["regression_gates"]["passed"],
            failed_gates=stats["regression_gates"]["failed"],
            pass_rate=stats["regression_gates"]["pass_rate"],
            avg_improvement_pct=stats["regression_gates"]["avg_improvement_pct"],
        )
        
    except Exception as e:
        logger.error(f"Get stats failed: {e}")
        raise HTTPException(status_code=500, detail=str(e))


@router.post("/feedback", response_model=FeedbackResponse)
async def submit_feedback(
    request: FeedbackRequest,
    db: Session = Depends(get_db_session),
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
        "timestamp": datetime.utcnow().isoformat(),
    }
