"""Admin API endpoints for system management."""

from datetime import datetime
from uuid import uuid4

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel
from sqlalchemy import func
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.models import AuditLog, Token, UserFeedback
from core.auth.encryption import encrypt_token
from core.auth.permission_checker import PermissionChecker

router = APIRouter(prefix="/admin", tags=["admin"])


# ============================================================================
# Request/Response Models
# ============================================================================


class InitResponse(BaseModel):
    """Database initialization response."""

    message: str
    tables_created: int


class TokenCreateRequest(BaseModel):
    """Token creation request."""

    token_type: str  # "llm" or "api"
    provider: str | None = None  # For LLM tokens: "openai", "anthropic", etc.
    scope: str = "global"  # "global", "account", "user"
    scope_id: str | None = None  # account_id or user_id
    token_value: str | None = None  # Actual token value


class TokenResponse(BaseModel):
    """Token response."""

    token_id: str
    token_type: str
    provider: str | None
    scope: str
    scope_id: str | None
    created_at: datetime


class AuditLogResponse(BaseModel):
    """Audit log response."""

    log_id: str
    user_id: str
    action: str
    resource_type: str
    resource_id: str | None
    timestamp: datetime
    details: dict | None  # Changed from metadata to details


class PromptOptimizeRequest(BaseModel):
    """Prompt optimization request."""

    agent_id: str
    optimization_type: str = "compression"  # "compression", "expansion", "clarification"


class PromptOptimizeResponse(BaseModel):
    """Prompt optimization response."""

    job_id: str
    status: str
    message: str


class FeedbackStatsResponse(BaseModel):
    """Feedback statistics response."""

    total_feedback: int
    positive_feedback: int
    negative_feedback: int
    avg_rating: float | None
    feedback_by_type: dict[str, int]


class FeedbackExportRequest(BaseModel):
    """Feedback export request."""

    agent_id: str | None = None
    format: str = "jsonl"  # "jsonl", "csv", "parquet"


class FeedbackExportResponse(BaseModel):
    """Feedback export response."""

    job_id: str
    status: str
    download_url: str | None


# ============================================================================
# Dependency: Require Admin Role
# ============================================================================


def require_admin(
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
) -> dict:
    """Verify user has admin role using PermissionChecker."""
    user_id = current_user["user_id"]
    checker = PermissionChecker(db)

    if not checker.is_admin(user_id):
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Admin role required",
        )

    return current_user


# ============================================================================
# Endpoints
# ============================================================================


@router.post("/init", response_model=InitResponse)
def init_database(
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> InitResponse:
    """Initialize database (run DDL migrations).

    Creates all required tables if they don't exist.
    """
    from api.database import init_db as run_init_db

    try:
        run_init_db()
        return InitResponse(
            message="Database initialized successfully",
            tables_created=0,  # init_db doesn't return count
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"Database initialization failed: {e!s}",
        ) from e


@router.post("/tokens", response_model=TokenResponse, status_code=status.HTTP_201_CREATED)
def create_token(
    request: TokenCreateRequest,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> TokenResponse:
    """Create API/LLM token with encryption."""
    token_id = str(uuid4())

    # Encrypt token value if provided
    encrypted_value = encrypt_token(request.token_value) if request.token_value else None

    # Create token using ORM
    token = Token(
        token_id=token_id,
        type=request.token_type,
        provider=request.provider or "unknown",
        encrypted_value=encrypted_value,
        is_active=1,
        scope_user_id=request.scope_id if request.scope == "user" else None,
        scope_repo=request.scope_id if request.scope == "repo" else None,
        token_metadata={"scope": request.scope},
    )

    db.add(token)
    db.commit()
    db.refresh(token)

    return TokenResponse(
        token_id=token.token_id,
        token_type=token.type,
        provider=token.provider,
        scope=request.scope,
        scope_id=token.scope_user_id or token.scope_repo,
        created_at=token.created_at,
    )


@router.get("/tokens", response_model=list[TokenResponse])
def list_tokens(
    token_type: str | None = None,
    scope: str | None = None,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> list[TokenResponse]:
    """List tokens using ORM."""
    query = db.query(Token)

    if token_type:
        query = query.filter(Token.type == token_type)

    if scope:
        if scope == "user":
            query = query.filter(Token.scope_user_id.isnot(None))
        elif scope == "repo":
            query = query.filter(Token.scope_repo.isnot(None))
        elif scope == "global":
            query = query.filter(Token.scope_user_id.is_(None), Token.scope_repo.is_(None))

    tokens = query.order_by(Token.created_at.desc()).all()

    return [
        TokenResponse(
            token_id=token.token_id,
            token_type=token.type,
            provider=token.provider,
            scope=(
                "user" if token.scope_user_id
                else "repo" if token.scope_repo
                else "global"
            ),
            scope_id=token.scope_user_id or token.scope_repo,
            created_at=token.created_at,
        )
        for token in tokens
    ]


@router.get("/audit", response_model=list[AuditLogResponse])
def get_audit_logs(
    user_id: str | None = None,
    since: str | None = None,
    limit: int = 100,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> list[AuditLogResponse]:
    """Query audit logs using ORM."""
    query = db.query(AuditLog)

    if user_id:
        query = query.filter(AuditLog.user_id == user_id)

    if since:
        query = query.filter(AuditLog.created_at >= since)

    logs = query.order_by(AuditLog.created_at.desc()).limit(limit).all()

    return [
        AuditLogResponse(
            log_id=log.log_id,
            user_id=log.user_id,
            action=log.action,
            resource_type=log.resource_type,
            resource_id=log.resource_id,
            timestamp=log.created_at,
            details=log.details,  # Already a dict from JSON column
        )
        for log in logs
    ]


@router.post("/prompts/optimize", response_model=PromptOptimizeResponse)
def optimize_prompt(
    request: PromptOptimizeRequest,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> PromptOptimizeResponse:
    """Trigger prompt optimization.

    This is a placeholder that would integrate with a prompt optimization service.
    """
    job_id = str(uuid4())

    # TODO: Integrate with actual prompt optimization service
    # For now, just return a job ID

    return PromptOptimizeResponse(
        job_id=job_id,
        status="queued",
        message=f"Prompt optimization job {job_id} queued for agent {request.agent_id}",
    )


@router.get("/feedback/stats", response_model=FeedbackStatsResponse)
def get_feedback_stats(
    agent_id: str | None = None,
    since: str | None = None,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> FeedbackStatsResponse:
    """Get feedback statistics using ORM."""
    query = db.query(
        func.count(UserFeedback.feedback_id).label("total"),
        func.sum(func.if_(UserFeedback.rating >= 4, 1, 0)).label("positive"),
        func.sum(func.if_(UserFeedback.rating <= 2, 1, 0)).label("negative"),
        func.avg(UserFeedback.rating).label("avg_rating"),
    )

    if agent_id:
        query = query.filter(UserFeedback.agent_id == agent_id)
    if since:
        query = query.filter(UserFeedback.created_at >= since)

    result = query.one()

    # Get feedback by type
    type_query = db.query(
        UserFeedback.feedback_type,
        func.count(UserFeedback.feedback_id).label("count"),
    ).filter(UserFeedback.feedback_type.isnot(None))

    if agent_id:
        type_query = type_query.filter(UserFeedback.agent_id == agent_id)
    if since:
        type_query = type_query.filter(UserFeedback.created_at >= since)

    type_results = type_query.group_by(UserFeedback.feedback_type).all()
    feedback_by_type = {row.feedback_type: row.count for row in type_results}

    return FeedbackStatsResponse(
        total_feedback=result.total or 0,
        positive_feedback=result.positive or 0,
        negative_feedback=result.negative or 0,
        avg_rating=float(result.avg_rating) if result.avg_rating else None,
        feedback_by_type=feedback_by_type,
    )


@router.post("/feedback/export", response_model=FeedbackExportResponse)
def export_feedback(
    request: FeedbackExportRequest,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> FeedbackExportResponse:
    """Export training data.

    This is a placeholder that would integrate with a data export service.
    """
    job_id = str(uuid4())

    # TODO: Integrate with actual data export service
    # For now, just return a job ID

    return FeedbackExportResponse(
        job_id=job_id,
        status="queued",
        download_url=None,
    )
