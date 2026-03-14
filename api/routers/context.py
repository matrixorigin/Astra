"""Context API Router - 上下文快照管理"""

from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query, status
from pydantic import BaseModel

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.services.context_service import ContextService
from api.services.exceptions import PermissionDeniedError, ResourceNotFoundError

router = APIRouter()


class CreateSnapshotRequest(BaseModel):
    """创建快照请求"""

    session_id: str
    event_id: str
    context_data: dict[str, Any]


class SnapshotResponse(BaseModel):
    """快照响应"""

    context_capture_id: str
    session_id: str
    event_id: str
    context_data: dict[str, Any]
    created_at: str


class SnapshotListResponse(BaseModel):
    """快照列表响应"""

    snapshots: list[dict[str, Any]]
    total: int
    limit: int
    offset: int


@router.post(
    "",
    response_model=SnapshotResponse,
    status_code=status.HTTP_201_CREATED,
    summary="创建上下文快照",
)
async def create_snapshot(
    request: CreateSnapshotRequest, current_user: dict = Depends(get_current_user)
):
    """创建上下文快照"""
    try:
        service = ContextService(SessionLocal)
        result = service.create_snapshot(
            user_id=current_user["user_id"],
            session_id=request.session_id,
            event_id=request.event_id,
            context_data=request.context_data,
        )
        return result
    except PermissionDeniedError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"创建快照失败: {e!s}"
        )


@router.get("", response_model=SnapshotListResponse, summary="列出上下文快照")
async def list_snapshots(
    session_id: str | None = Query(None),
    limit: int = Query(50, ge=1, le=100),
    offset: int = Query(0, ge=0),
    current_user: dict = Depends(get_current_user),
):
    """列出上下文快照"""
    try:
        service = ContextService(SessionLocal)
        return service.list_snapshots(
            user_id=current_user["user_id"], session_id=session_id, limit=limit, offset=offset
        )
    except PermissionDeniedError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))


@router.get("/{context_capture_id}", response_model=SnapshotResponse, summary="获取上下文快照")
async def get_snapshot(context_capture_id: str, current_user: dict = Depends(get_current_user)):
    """获取上下文快照"""
    try:
        service = ContextService(SessionLocal)
        return service.get_snapshot(
            context_capture_id=context_capture_id, user_id=current_user["user_id"]
        )
    except ResourceNotFoundError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
