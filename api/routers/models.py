"""LLM Model management API — admin registers models with API keys, validates connectivity."""

import logging
import re
from uuid import uuid4

import httpx
from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel
from sqlalchemy.orm import Session

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.models import LLMModel
from core.auth.encryption import decrypt_token, encrypt_token
from core.llm.constants import PROVIDER_BASE_URLS

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/models", tags=["models"])


# ── Schemas ──


class PricingSchema(BaseModel):
    prompt: float = 0.0
    completion: float = 0.0
    cache_read: float | None = None
    cache_write: float | None = None


class ModelCreateRequest(BaseModel):
    name: str
    provider: str
    api_key: str  # required — the whole point
    base_url: str | None = None  # override provider default
    context_window: int = 128000
    max_completion_tokens: int | None = None
    input_modalities: list[str] = ["text"]
    output_modalities: list[str] = ["text"]
    supported_parameters: list[str] = []
    pricing: PricingSchema = PricingSchema()
    architecture: str | None = None
    tags: list[str] = []


class ModelUpdateRequest(BaseModel):
    api_key: str | None = None
    base_url: str | None = None
    context_window: int | None = None
    max_completion_tokens: int | None = None
    input_modalities: list[str] | None = None
    output_modalities: list[str] | None = None
    supported_parameters: list[str] | None = None
    pricing: PricingSchema | None = None
    architecture: str | None = None
    tags: list[str] | None = None
    is_active: bool | None = None


class ModelResponse(BaseModel):
    model_id: str
    name: str
    provider: str
    base_url: str | None = None
    is_active: bool = True
    context_window: int = 128000
    max_completion_tokens: int | None = None
    input_modalities: list[str] = ["text"]
    output_modalities: list[str] = ["text"]
    supported_parameters: list[str] = []
    pricing: PricingSchema = PricingSchema()
    architecture: str | None = None
    tags: list[str] = []
    connectivity: str | None = None  # "ok" or error message, only on create/update


# ── Helpers ──


def _require_admin(current_user: dict, db: Session):
    from core.auth.permission_checker import PermissionChecker
    if not PermissionChecker(lambda: db).is_admin(current_user["user_id"]):
        raise HTTPException(status_code=403, detail="Admin role required")


def _resolve_base_url(provider: str, explicit: str | None) -> str | None:
    """Resolve base_url: explicit > well-known default."""
    return explicit or PROVIDER_BASE_URLS.get(provider)


def _sanitize_error(msg: str) -> str:
    """Remove potential secrets (API keys, tokens) from error messages."""
    msg = re.sub(r'(sk-[a-zA-Z0-9-]{0,6})[a-zA-Z0-9-]+', r'\1...', msg)
    msg = re.sub(r'(?<![a-zA-Z0-9])[a-zA-Z0-9]{32,}(?![a-zA-Z0-9])', '<redacted>', msg)
    return msg[:200]


def _validate_connectivity(provider: str, model_name: str, api_key: str, base_url: str | None) -> str | None:
    """Quick connectivity check — send a tiny request. Returns None on success, error string on failure."""
    if provider == "mock":
        return None
    try:
        if provider == "anthropic" and not base_url:
            r = httpx.post(
                "https://api.anthropic.com/v1/messages",
                headers={
                    "x-api-key": api_key,
                    "anthropic-version": "2023-06-01",
                    "content-type": "application/json",
                },
                json={"model": model_name, "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]},
                timeout=15.0,
            )
        else:
            url = (base_url or "https://api.openai.com/v1").rstrip("/")
            r = httpx.post(
                f"{url}/chat/completions",
                headers={"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"},
                json={"model": model_name, "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]},
                timeout=15.0,
            )
        if r.status_code < 400:
            return None
        try:
            detail = r.json().get("error", {}).get("message", r.text[:200])
        except Exception:
            detail = r.text[:200]
        return f"HTTP {r.status_code}: {_sanitize_error(detail)}"
    except httpx.ConnectError as e:
        return f"Connection failed: {_sanitize_error(str(e))}"
    except httpx.TimeoutException:
        return "Connection timed out (15s)"
    except Exception as e:
        return f"Unexpected error: {_sanitize_error(str(e))}"


def _to_response(m: LLMModel, connectivity: str | None = None) -> ModelResponse:
    return ModelResponse(
        model_id=m.model_id, name=m.model_name, provider=m.provider,
        base_url=m.base_url, is_active=bool(m.is_active),
        context_window=m.context_window or 128000,
        max_completion_tokens=m.max_completion_tokens,
        input_modalities=m.input_modalities or ["text"],
        output_modalities=m.output_modalities or ["text"],
        supported_parameters=m.supported_parameters or [],
        pricing=PricingSchema(**(m.pricing or {})),
        architecture=m.architecture, tags=m.tags or [],
        connectivity=connectivity,
    )


# ── Endpoints ──


@router.post("", response_model=ModelResponse, status_code=status.HTTP_201_CREATED)
def create_model(
    request: ModelCreateRequest,
    current_user: dict = Depends(get_current_user),
):
    """Register a new model with API key. Validates connectivity."""
    db = SessionLocal()
    try:
        _require_admin(current_user, db)
        existing = db.query(LLMModel).filter(
            LLMModel.model_name == request.name, LLMModel.provider == request.provider,
        ).first()
        if existing:
            raise HTTPException(status_code=400, detail=f"Model '{request.name}' ({request.provider}) already exists")

        base_url = _resolve_base_url(request.provider, request.base_url)
        conn_err = _validate_connectivity(request.provider, request.name, request.api_key, base_url)

        model = LLMModel(
            model_id=str(uuid4()), model_name=request.name, provider=request.provider,
            api_key_encrypted=encrypt_token(request.api_key), base_url=base_url,
            is_active=1 if conn_err is None else 0,
            context_window=request.context_window, max_completion_tokens=request.max_completion_tokens,
            input_modalities=request.input_modalities, output_modalities=request.output_modalities,
            supported_parameters=request.supported_parameters,
            pricing=request.pricing.model_dump(exclude_none=True),
            architecture=request.architecture, tags=request.tags,
            created_by=current_user["user_id"],
        )
        db.add(model)
        db.commit()
        db.refresh(model)

        connectivity = "ok" if conn_err is None else conn_err
        if conn_err:
            logger.warning(f"Model '{request.name}' registered as inactive: {conn_err}")
        return _to_response(model, connectivity=connectivity)
    finally:
        db.close()


@router.get("", response_model=list[ModelResponse])
def list_models(
    current_user: dict = Depends(get_current_user),
):
    db = SessionLocal()
    try:
        from core.auth.permission_checker import PermissionChecker
        is_admin = PermissionChecker(lambda: db).is_admin(current_user["user_id"])
        query = db.query(LLMModel)
        if not is_admin:
            query = query.filter(LLMModel.is_active == 1)
        models = query.order_by(LLMModel.provider, LLMModel.model_name).all()
        return [_to_response(m) for m in models]
    finally:
        db.close()


@router.get("/{model_name}", response_model=ModelResponse)
def get_model(
    model_name: str,
    current_user: dict = Depends(get_current_user),
):
    db = SessionLocal()
    try:
        model = db.query(LLMModel).filter(LLMModel.model_name == model_name).first()
        if not model:
            raise HTTPException(status_code=404, detail=f"Model '{model_name}' not found")
        return _to_response(model)
    finally:
        db.close()


@router.put("/{model_name}", response_model=ModelResponse)
def update_model(
    model_name: str,
    request: ModelUpdateRequest,
    current_user: dict = Depends(get_current_user),
):
    """Update model config or API key. Re-validates connectivity if key changes."""
    db = SessionLocal()
    try:
        _require_admin(current_user, db)
        model = db.query(LLMModel).filter(LLMModel.model_name == model_name).first()
        if not model:
            raise HTTPException(status_code=404, detail=f"Model '{model_name}' not found")

        conn_result = None
        if request.api_key is not None:
            model.api_key_encrypted = encrypt_token(request.api_key)
            base_url = request.base_url or model.base_url
            conn_err = _validate_connectivity(model.provider, model.model_name, request.api_key, base_url)
            conn_result = "ok" if conn_err is None else conn_err
            if request.is_active is None:
                model.is_active = 1 if conn_err is None else 0

        if request.base_url is not None:
            model.base_url = request.base_url
        if request.context_window is not None:
            model.context_window = request.context_window
        if request.max_completion_tokens is not None:
            model.max_completion_tokens = request.max_completion_tokens
        if request.input_modalities is not None:
            model.input_modalities = request.input_modalities
        if request.output_modalities is not None:
            model.output_modalities = request.output_modalities
        if request.supported_parameters is not None:
            model.supported_parameters = request.supported_parameters
        if request.pricing is not None:
            model.pricing = request.pricing.model_dump(exclude_none=True)
        if request.architecture is not None:
            model.architecture = request.architecture
        if request.tags is not None:
            model.tags = request.tags
        if request.is_active is not None:
            model.is_active = 1 if request.is_active else 0

        db.commit()
        db.refresh(model)
        return _to_response(model, connectivity=conn_result)
    finally:
        db.close()


@router.delete("/{model_name}", status_code=status.HTTP_204_NO_CONTENT)
def delete_model(
    model_name: str,
    current_user: dict = Depends(get_current_user),
):
    db = SessionLocal()
    try:
        _require_admin(current_user, db)
        model = db.query(LLMModel).filter(LLMModel.model_name == model_name).first()
        if not model:
            raise HTTPException(status_code=404, detail=f"Model '{model_name}' not found")
        db.delete(model)
        db.commit()
    finally:
        db.close()


@router.post("/{model_name}/check", response_model=ModelResponse)
def check_model(
    model_name: str,
    current_user: dict = Depends(get_current_user),
):
    """Re-check model connectivity and update active status."""
    db = SessionLocal()
    try:
        _require_admin(current_user, db)
        model = db.query(LLMModel).filter(LLMModel.model_name == model_name).first()
        if not model:
            raise HTTPException(status_code=404, detail=f"Model '{model_name}' not found")
        api_key = decrypt_token(model.api_key_encrypted)
        conn_err = _validate_connectivity(model.provider, model.model_name, api_key, model.base_url)
        model.is_active = 1 if conn_err is None else 0
        db.commit()
        db.refresh(model)
        return _to_response(model, connectivity="ok" if conn_err is None else conn_err)
    finally:
        db.close()
