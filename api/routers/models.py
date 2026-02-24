"""Models management API endpoints."""

import json
from uuid import uuid4

from enum import Enum

from fastapi import APIRouter, Depends, HTTPException, Query, status
from pydantic import BaseModel
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user

router = APIRouter(prefix="/models", tags=["models"])


class ModelScope(str, Enum):
    GLOBAL = "global"
    USER = "user"


def _require_admin_for_global(scope: str, current_user: dict, db: Session):
    """Require admin role for global scope writes."""
    if scope != "global":
        return
    from core.auth.permission_checker import PermissionChecker
    if not PermissionChecker(db).is_admin(current_user["user_id"]):
        raise HTTPException(status_code=403, detail="Admin role required for global scope")


# ── Request / Response schemas ──


class PricingRequest(BaseModel):
    prompt: float = 0.0
    completion: float = 0.0
    cache_read: float | None = None
    cache_write: float | None = None
    image: float | None = None
    request: float | None = None


class ModelCreateRequest(BaseModel):
    name: str
    provider: str
    scope: ModelScope = ModelScope.GLOBAL
    context_window: int = 128000
    max_completion_tokens: int | None = None
    input_modalities: list[str] = ["text"]
    output_modalities: list[str] = ["text"]
    supported_parameters: list[str] = []
    pricing: PricingRequest = PricingRequest()
    architecture: str | None = None
    parameter_count: str | None = None
    tags: list[str] = []


class ModelUpdateRequest(BaseModel):
    provider: str | None = None
    context_window: int | None = None
    max_completion_tokens: int | None = None
    input_modalities: list[str] | None = None
    output_modalities: list[str] | None = None
    supported_parameters: list[str] | None = None
    pricing: PricingRequest | None = None
    architecture: str | None = None
    parameter_count: str | None = None
    tags: list[str] | None = None
    is_active: bool | None = None


class PricingResponse(BaseModel):
    prompt: float = 0.0
    completion: float = 0.0
    cache_read: float | None = None
    cache_write: float | None = None
    image: float | None = None
    request: float | None = None


class ModelResponse(BaseModel):
    name: str
    provider: str
    scope: str
    context_window: int = 128000
    max_completion_tokens: int | None = None
    input_modalities: list[str] = ["text"]
    output_modalities: list[str] = ["text"]
    supported_parameters: list[str] = []
    pricing: PricingResponse = PricingResponse()
    architecture: str | None = None
    parameter_count: str | None = None
    tags: list[str] = []
    is_active: bool = True


# ── Helpers ──


def _load_registry(db, scope: str, scope_user_id: str | None):
    """Load model registry JSON array from configs table."""
    result = db.execute(
        text("""
            SELECT config_id, value FROM configs
            WHERE key_name = 'model_registry'
            AND scope_type = :scope
            AND (scope_user_id = :uid OR (scope_user_id IS NULL AND :uid IS NULL))
            LIMIT 1
        """),
        {"scope": scope, "uid": scope_user_id},
    ).fetchone()
    if result:
        return result[0], json.loads(result[1])
    return None, []


def _save_registry(db, config_id: str | None, models: list, scope: str, scope_user_id: str | None):
    """Upsert model registry JSON array into configs table."""
    if config_id:
        db.execute(
            text("UPDATE configs SET value = :value WHERE config_id = :id"),
            {"value": json.dumps(models), "id": config_id},
        )
    else:
        db.execute(
            text("""
                INSERT INTO configs (config_id, key_name, value, scope_type, scope_user_id)
                VALUES (:id, 'model_registry', :value, :scope, :uid)
            """),
            {"id": str(uuid4()), "value": json.dumps(models), "scope": scope, "uid": scope_user_id},
        )
    db.commit()


def _model_to_response(m: dict, scope: str) -> ModelResponse:
    """Convert stored model dict to API response."""
    provider = m.get("provider", "")
    provider_str = provider.value if hasattr(provider, "value") else str(provider)

    # Handle both nested pricing and old flat fields
    pricing_data = m.get("pricing", {})
    if not pricing_data:
        pricing_data = {
            "prompt": m.get("price_per_1k_prompt", 0.0),
            "completion": m.get("price_per_1k_completion", 0.0),
        }

    return ModelResponse(
        name=m["model_name"],
        provider=provider_str,
        scope=scope,
        context_window=m.get("context_window", 128000),
        max_completion_tokens=m.get("max_completion_tokens"),
        input_modalities=m.get("input_modalities", ["text"]),
        output_modalities=m.get("output_modalities", ["text"]),
        supported_parameters=m.get("supported_parameters", []),
        pricing=PricingResponse(**pricing_data),
        architecture=m.get("architecture"),
        parameter_count=m.get("parameter_count"),
        tags=m.get("tags", []),
        is_active=m.get("is_active", True),
    )


# ── Endpoints ──


@router.post("", response_model=ModelResponse, status_code=status.HTTP_201_CREATED)
def create_model(
    request: ModelCreateRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Register a new model."""
    _require_admin_for_global(request.scope, current_user, db)
    scope_user_id = current_user["user_id"] if request.scope == "user" else None
    config_id, models = _load_registry(db, request.scope, scope_user_id)

    if any(m["model_name"] == request.name for m in models):
        raise HTTPException(status_code=400, detail=f"Model '{request.name}' already exists in {request.scope} scope")

    model_config = {
        "model_name": request.name,
        "provider": request.provider,
        "context_window": request.context_window,
        "max_completion_tokens": request.max_completion_tokens,
        "input_modalities": request.input_modalities,
        "output_modalities": request.output_modalities,
        "supported_parameters": request.supported_parameters,
        "pricing": request.pricing.model_dump(exclude_none=True),
        "architecture": request.architecture,
        "parameter_count": request.parameter_count,
        "tags": request.tags,
        "is_active": True,
    }
    models.append(model_config)
    _save_registry(db, config_id, models, request.scope, scope_user_id)

    return _model_to_response(model_config, request.scope)


@router.get("", response_model=list[ModelResponse])
def list_models(
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """List all models available to the current user."""
    from core.llm.router import ModelRegistry

    user_id = current_user.get("user_id")
    registry = ModelRegistry()
    registry.load_from_db(db, user_id)

    result = []
    for m in registry.list_active():
        provider = m.provider.value if hasattr(m.provider, "value") else str(m.provider)
        result.append(ModelResponse(
            name=m.model_name,
            provider=provider,
            scope="global",
            context_window=m.context_window,
            max_completion_tokens=m.max_completion_tokens,
            input_modalities=m.input_modalities,
            output_modalities=m.output_modalities,
            supported_parameters=m.supported_parameters,
            pricing=PricingResponse(**m.pricing.model_dump()),
            architecture=m.architecture,
            parameter_count=m.parameter_count,
            tags=m.tags,
            is_active=m.is_active,
        ))
    return result


@router.put("/{model_name}", response_model=ModelResponse)
def update_model(
    model_name: str,
    request: ModelUpdateRequest,
    scope: ModelScope = Query(default=ModelScope.GLOBAL),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Update an existing model."""
    _require_admin_for_global(scope, current_user, db)
    scope_user_id = current_user["user_id"] if scope == "user" else None
    config_id, models = _load_registry(db, scope, scope_user_id)

    if not config_id:
        raise HTTPException(status_code=404, detail="Model registry not found")

    target = next((m for m in models if m["model_name"] == model_name), None)
    if not target:
        raise HTTPException(status_code=404, detail=f"Model '{model_name}' not found")

    updates = request.model_dump(exclude_none=True)
    if "pricing" in updates:
        updates["pricing"] = updates["pricing"].model_dump(exclude_none=True) if hasattr(updates["pricing"], "model_dump") else updates["pricing"]
    target.update(updates)

    _save_registry(db, config_id, models, scope, scope_user_id)
    return _model_to_response(target, scope)


@router.delete("/{model_name}", status_code=status.HTTP_204_NO_CONTENT)
def delete_model(
    model_name: str,
    scope: ModelScope = Query(default=ModelScope.GLOBAL),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Delete a model from the registry."""
    _require_admin_for_global(scope, current_user, db)
    scope_user_id = current_user["user_id"] if scope == "user" else None
    config_id, models = _load_registry(db, scope, scope_user_id)

    if not config_id:
        raise HTTPException(status_code=404, detail="Model registry not found")

    new_models = [m for m in models if m["model_name"] != model_name]
    if len(new_models) == len(models):
        raise HTTPException(status_code=404, detail=f"Model '{model_name}' not found")

    _save_registry(db, config_id, new_models, scope, scope_user_id)
