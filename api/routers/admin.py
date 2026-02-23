"""Admin API endpoints for system management."""

from datetime import datetime
from uuid import uuid4

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user

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
    metadata: dict | None


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
    """Verify user has admin role."""
    user_id = current_user["user_id"]

    # Check if user has mo_agent_admin role
    result = db.execute(
        text("""
            SELECT r.role_name
            FROM user_roles ur
            JOIN roles r ON ur.role_id = r.role_id
            WHERE ur.user_id = :user_id AND r.role_name = 'mo_agent_admin'
        """),
        {"user_id": user_id},
    ).fetchone()

    if not result:
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
        return InitResponse(
            message=f"Database initialization completed with warnings: {e!s}",
            tables_created=0,
        )


@router.post("/tokens", response_model=TokenResponse, status_code=status.HTTP_201_CREATED)
def create_token(
    request: TokenCreateRequest,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> TokenResponse:
    """Create API/LLM token."""
    try:
        token_id = str(uuid4())

        # Insert token into existing tokens table
        db.execute(
            text("""
                INSERT INTO tokens (
                    token_id, type, provider, encrypted_value, is_active,
                    scope_user_id, scope_repo, created_at, metadata
                ) VALUES (
                    :token_id, :type, :provider, :encrypted_value, 1,
                    :scope_user_id, :scope_repo, NOW(), :metadata
                )
            """),
            {
                "token_id": token_id,
                "type": request.token_type,
                "provider": request.provider or "unknown",
                "encrypted_value": request.token_value or "",
                "scope_user_id": request.scope_id if request.scope == "user" else None,
                "scope_repo": request.scope_id if request.scope == "repo" else None,
                "metadata": '{"scope": "' + request.scope + '"}',
            },
        )
        db.commit()

        # Fetch created token
        result = db.execute(
            text("SELECT * FROM tokens WHERE token_id = :token_id"),
            {"token_id": token_id},
        ).fetchone()

        return TokenResponse(
            token_id=result.token_id,
            token_type=result.type,
            provider=result.provider,
            scope=request.scope,
            scope_id=result.scope_user_id or result.scope_repo,
            created_at=result.created_at,
        )
    except Exception:
        db.rollback()
        raise


@router.get("/tokens", response_model=list[TokenResponse])
def list_tokens(
    token_type: str | None = None,
    scope: str | None = None,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> list[TokenResponse]:
    """List tokens."""
    query = """
        SELECT token_id, type, provider, scope_user_id, scope_repo, created_at,
               CASE
                   WHEN scope_user_id IS NOT NULL THEN 'user'
                   WHEN scope_repo IS NOT NULL THEN 'repo'
                   ELSE 'global'
               END as scope_type,
               COALESCE(scope_user_id, scope_repo) as scope_id
        FROM tokens WHERE 1=1
    """
    params = {}

    if token_type:
        query += " AND type = :token_type"
        params["token_type"] = token_type

    if scope:
        if scope == "user":
            query += " AND scope_user_id IS NOT NULL"
        elif scope == "repo":
            query += " AND scope_repo IS NOT NULL"
        elif scope == "global":
            query += " AND scope_user_id IS NULL AND scope_repo IS NULL"

    query += " ORDER BY created_at DESC"

    results = db.execute(text(query), params).fetchall()

    return [
        TokenResponse(
            token_id=row.token_id,
            token_type=row.type,
            provider=row.provider,
            scope=row.scope_type,
            scope_id=row.scope_id,
            created_at=row.created_at,
        )
        for row in results
    ]


@router.get("/audit", response_model=list[AuditLogResponse])
def get_audit_logs(
    user_id: str | None = None,
    since: str | None = None,
    limit: int = 100,
    db: Session = Depends(get_db_session),
    _admin: dict = Depends(require_admin),
) -> list[AuditLogResponse]:
    """Query audit logs."""
    query = "SELECT * FROM audit_logs WHERE 1=1"
    params = {"limit": limit}

    if user_id:
        query += " AND user_id = :user_id"
        params["user_id"] = user_id

    if since:
        query += " AND created_at >= :since"
        params["since"] = since

    query += " ORDER BY created_at DESC LIMIT :limit"

    results = db.execute(text(query), params).fetchall()

    import json
    return [
        AuditLogResponse(
            log_id=row.log_id,
            user_id=row.user_id,
            action=row.action,
            resource_type=row.resource_type,
            resource_id=row.resource_id,
            timestamp=row.created_at,
            metadata=json.loads(row.details) if hasattr(row, 'details') and row.details else None,
        )
        for row in results
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
    """Get feedback statistics."""
    try:
        # Create user_feedback table if not exists
        db.execute(
            text("""
                CREATE TABLE IF NOT EXISTS user_feedback (
                    feedback_id VARCHAR(64) PRIMARY KEY,
                    user_id VARCHAR(255) NOT NULL,
                    agent_id VARCHAR(255),
                    session_id VARCHAR(64),
                    event_id VARCHAR(64),
                    rating INT,
                    feedback_type VARCHAR(32),
                    comment TEXT,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    INDEX idx_agent (agent_id, created_at),
                    INDEX idx_user (user_id, created_at)
                )
            """)
        )
        db.commit()

        query = """
            SELECT
                COUNT(*) as total,
                SUM(CASE WHEN rating >= 4 THEN 1 ELSE 0 END) as positive,
                SUM(CASE WHEN rating <= 2 THEN 1 ELSE 0 END) as negative,
                AVG(rating) as avg_rating
            FROM user_feedback
            WHERE 1=1
        """
        params = {}

        if agent_id:
            query += " AND agent_id = :agent_id"
            params["agent_id"] = agent_id

        if since:
            query += " AND created_at >= :since"
            params["since"] = since

        result = db.execute(text(query), params).fetchone()

        # Get feedback by type
        type_query = """
            SELECT feedback_type, COUNT(*) as count
            FROM user_feedback
            WHERE 1=1
        """
        if agent_id:
            type_query += " AND agent_id = :agent_id"
        if since:
            type_query += " AND created_at >= :since"
        type_query += " GROUP BY feedback_type"

        type_results = db.execute(text(type_query), params).fetchall()
        feedback_by_type = {row.feedback_type: row.count for row in type_results if row.feedback_type}

        return FeedbackStatsResponse(
            total_feedback=result.total or 0,
            positive_feedback=result.positive or 0,
            negative_feedback=result.negative or 0,
            avg_rating=float(result.avg_rating) if result.avg_rating else None,
            feedback_by_type=feedback_by_type,
        )
    except Exception:
        db.rollback()
        raise


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
