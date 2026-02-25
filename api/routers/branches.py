"""Branch API — zero-copy data branching (diff, merge, cost estimation)."""

from __future__ import annotations

import re
from typing import Annotated, Any, Literal

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import AfterValidator, BaseModel, Field
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from core.logging_config import get_logger

logger = get_logger(__name__)

router = APIRouter(prefix="/api/v1/branches", tags=["branches"])

# Only allow safe identifiers: letters, digits, underscores, dots (for db.table)
_SAFE_IDENT = re.compile(r"^[a-zA-Z_][a-zA-Z0-9_.]{0,127}$")


def _check_ident(v: str) -> str:
    if not _SAFE_IDENT.match(v):
        raise ValueError("Invalid identifier: must match [a-zA-Z_][a-zA-Z0-9_.]*")
    return v


SafeIdent = Annotated[str, AfterValidator(_check_ident)]


# ---------------------------------------------------------------------------
# Request / Response models
# ---------------------------------------------------------------------------

class CreateBranchRequest(BaseModel):
    name: SafeIdent = Field(..., description="Branch (target) table or database name")
    source: SafeIdent = Field(..., description="Source table or database name")
    snapshot: SafeIdent | None = Field(default=None, description="Optional snapshot name for point-in-time branch")
    is_database: bool = Field(default=False, description="Branch a full database instead of a single table")


class DiffRequest(BaseModel):
    target: SafeIdent
    source: SafeIdent
    target_snapshot: SafeIdent | None = None
    source_snapshot: SafeIdent | None = None
    output: Literal["default", "count"] = Field(default="default")


class MergeRequest(BaseModel):
    source: SafeIdent
    target: SafeIdent
    on_conflict: Literal["skip", "accept", "error"] = Field(default="skip")


class DeleteBranchRequest(BaseModel):
    name: SafeIdent
    is_database: bool = False


class CostEstimateRequest(BaseModel):
    operation: str = Field(..., description="create, delete, diff, or merge")
    model: str = Field(default="gpt-4o-mini")
    session_count: int = Field(default=1, ge=1)
    budget_remaining: float | None = None


class DiffResponse(BaseModel):
    rows: list[dict[str, Any]]
    count: int


class CostEstimateResponse(BaseModel):
    operation: str
    model: str
    estimated_tokens: int
    estimated_cost: float
    exceeds_budget: bool = False
    alternatives: list[dict[str, Any]] = []


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _get_branch(db: Session):
    from api.database import settings
    from core.sandbox.branch import Branch
    return Branch(database=settings.matrixone_database, db_factory=lambda: db)


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.post("", status_code=status.HTTP_201_CREATED)
def create_branch(
    request: CreateBranchRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Create a zero-copy branch from source."""
    try:
        _get_branch(db).create(
            name=request.name, source=request.source,
            snapshot=request.snapshot, is_database=request.is_database,
        )
        return {"status": "created", "name": request.name, "source": request.source}
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@router.post("/diff", response_model=DiffResponse)
def diff_branch(
    request: DiffRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Three-way diff between two tables/snapshots."""
    try:
        rows = _get_branch(db).diff(
            target=request.target, source=request.source,
            output=request.output,
            target_snapshot=request.target_snapshot,
            source_snapshot=request.source_snapshot,
        )
        return DiffResponse(rows=rows, count=len(rows))
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@router.post("/merge")
def merge_branch(
    request: MergeRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Merge source into target with conflict strategy."""
    try:
        _get_branch(db).merge(
            source=request.source, target=request.target,
            on_conflict=request.on_conflict,
        )
        return {"status": "merged", "source": request.source, "target": request.target}
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@router.delete("")
def delete_branch(
    request: DeleteBranchRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Delete a branch."""
    try:
        _get_branch(db).delete(name=request.name, is_database=request.is_database)
        return {"status": "deleted", "name": request.name}
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))


@router.post("/cost-estimate", response_model=CostEstimateResponse)
def estimate_cost(
    request: CostEstimateRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Estimate cost before running a branch operation."""
    from core.sandbox.cost_predictor import BranchCostPredictor

    est = BranchCostPredictor(lambda: db).estimate_branch(
        operation=request.operation, model=request.model,
        session_count=request.session_count,
        budget_remaining=request.budget_remaining,
    )
    return CostEstimateResponse(
        operation=est.operation, model=est.model,
        estimated_tokens=est.estimated_tokens,
        estimated_cost=est.estimated_cost,
        exceeds_budget=est.exceeds_budget,
        alternatives=est.alternatives,
    )
