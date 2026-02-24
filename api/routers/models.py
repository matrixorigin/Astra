"""Models management API endpoints."""

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy.orm import Session
from pydantic import BaseModel
from api.database import get_db_session
from api.dependencies import get_current_user
from api.models import User

router = APIRouter(prefix="/models", tags=["models"])


class ModelCreateRequest(BaseModel):
    """Model creation request."""
    name: str
    provider: str
    scope: str = "global"
    scope_id: str | None = None


class ModelResponse(BaseModel):
    """Model response."""
    name: str
    provider: str
    scope: str
    scope_id: str | None = None


@router.post("", response_model=ModelResponse, status_code=status.HTTP_201_CREATED)
def create_model(
    request: ModelCreateRequest,
    db: Session = Depends(get_db_session),
    current_user: User = Depends(get_current_user),
):
    """Register a new model."""
    return ModelResponse(
        name=request.name,
        provider=request.provider,
        scope=request.scope,
        scope_id=request.scope_id,
    )


@router.get("", response_model=list[ModelResponse])
def list_models(
    db: Session = Depends(get_db_session),
    current_user: User = Depends(get_current_user),
):
    """List all models."""
    return []
