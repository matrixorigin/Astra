"""Decision API Router - 决策审计"""

from typing import Dict, Any, Optional
from fastapi import APIRouter, Depends, HTTPException, status, Query
from pydantic import BaseModel
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.services.decision_service import DecisionService
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError


router = APIRouter()


class RecordDecisionRequest(BaseModel):
    """记录决策请求"""
    session_id: str
    event_id: str
    context_capture_id: str
    decision_type: str
    decision_output: Dict[str, Any]
    model_params: Optional[Dict[str, Any]] = None


class DecisionResponse(BaseModel):
    """决策响应"""
    decision_id: str
    session_id: str
    event_id: str
    context_capture_id: str
    decision_type: str
    decision_output: Dict[str, Any]
    model_params: Dict[str, Any]
    created_at: str


class DecisionWithContextResponse(BaseModel):
    """决策及上下文响应"""
    decision_id: str
    session_id: str
    event_id: str
    context_capture_id: str
    decision_type: str
    decision_output: Dict[str, Any]
    model_params: Dict[str, Any]
    context: Optional[Dict[str, Any]] = None
    created_at: str


class DecisionListResponse(BaseModel):
    """决策列表响应"""
    decisions: list[Dict[str, Any]]
    total: int
    limit: int
    offset: int


@router.post(
    "",
    response_model=DecisionResponse,
    status_code=status.HTTP_201_CREATED,
    summary="记录决策"
)
async def record_decision(
    request: RecordDecisionRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """记录决策"""
    try:
        service = DecisionService(db)
        result = service.record_decision(
            user_id=current_user["user_id"],
            session_id=request.session_id,
            event_id=request.event_id,
            context_capture_id=request.context_capture_id,
            decision_type=request.decision_type,
            decision_output=request.decision_output,
            model_params=request.model_params
        )
        return result
    except PermissionDeniedError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"记录决策失败: {str(e)}"
        )


@router.get(
    "",
    response_model=DecisionListResponse,
    summary="列出决策"
)
async def list_decisions(
    session_id: Optional[str] = Query(None),
    decision_type: Optional[str] = Query(None),
    limit: int = Query(50, ge=1, le=100),
    offset: int = Query(0, ge=0),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """列出决策"""
    service = DecisionService(db)
    return service.list_decisions(
        user_id=current_user["user_id"],
        session_id=session_id,
        decision_type=decision_type,
        limit=limit,
        offset=offset
    )


@router.get(
    "/{decision_id}",
    response_model=DecisionResponse,
    summary="获取决策"
)
async def get_decision(
    decision_id: str,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """获取决策"""
    try:
        service = DecisionService(db)
        return service.get_decision(
            decision_id=decision_id,
            user_id=current_user["user_id"]
        )
    except ResourceNotFoundError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))


@router.get(
    "/{decision_id}/audit",
    response_model=DecisionWithContextResponse,
    summary="审计决策（含完整上下文）"
)
async def audit_decision(
    decision_id: str,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """获取决策及其完整上下文，用于审计"""
    try:
        service = DecisionService(db)
        return service.get_decision_with_context(
            decision_id=decision_id,
            user_id=current_user["user_id"]
        )
    except ResourceNotFoundError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
