"""Session API Router - 使用服务层"""

from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query, status
from pydantic import BaseModel

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.services.session_service import SessionService

router = APIRouter()


# Request/Response Models
class CreateSessionRequest(BaseModel):
    """创建 Session 请求"""
    agent_id: str | None = None
    title: str | None = None
    metadata: dict[str, Any] | None = None


class UpdateSessionRequest(BaseModel):
    """更新 Session 请求"""
    title: str | None = None
    metadata: dict[str, Any] | None = None
    status: str | None = None


class SessionResponse(BaseModel):
    """Session 响应"""
    session_id: str
    user_id: str
    agent_id: str | None = None
    title: str | None = None  # Title is optional
    metadata: dict[str, Any]
    status: str
    event_count: int
    created_at: str
    updated_at: str | None = None
    ended_at: str | None = None


class SessionListResponse(BaseModel):
    """Session 列表响应"""
    sessions: list[SessionResponse]
    total: int
    limit: int
    offset: int


# API Endpoints
@router.post(
    "",
    response_model=SessionResponse,
    status_code=status.HTTP_201_CREATED,
    summary="创建 Session",
    description="创建一个新的会话"
)
async def create_session(
    request: CreateSessionRequest,
    current_user: dict = Depends(get_current_user)
):
    """创建 Session"""
    try:
        service = SessionService(SessionLocal)
        result = service.create_session(
            user_id=current_user["user_id"],
            agent_id=request.agent_id,
            title=request.title,
            metadata=request.metadata
        )
        return result
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"创建 Session 失败: {e!s}"
        )


@router.get(
    "",
    response_model=SessionListResponse,
    summary="列出 Sessions",
    description="列出当前用户的会话"
)
async def list_sessions(
    agent_id: str | None = Query(None, description="过滤Agent ID"),
    session_status: str | None = Query(None, description="过滤状态"),
    limit: int = Query(50, ge=1, le=100, description="限制数量"),
    offset: int = Query(0, ge=0, description="偏移量"),
    current_user: dict = Depends(get_current_user)
):
    """列出 Sessions"""
    try:
        service = SessionService(SessionLocal)
        result = service.list_sessions(
            user_id=current_user["user_id"],
            agent_id=agent_id,
            status=session_status,
            limit=limit,
            offset=offset
        )
        return result
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"获取 Sessions 失败: {e!s}"
        )


@router.get(
    "/{session_id}",
    response_model=SessionResponse,
    summary="获取 Session",
    description="获取指定会话的详细信息"
)
async def get_session(
    session_id: str,
    current_user: dict = Depends(get_current_user)
):
    """获取 Session"""
    try:
        service = SessionService(SessionLocal)
        result = service.get_session(session_id=session_id, user_id=current_user["user_id"])
        return result
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"获取 Session 失败: {e!s}"
        )


@router.put(
    "/{session_id}",
    response_model=SessionResponse,
    summary="更新 Session",
    description="更新指定会话的信息"
)
async def update_session(
    session_id: str,
    request: UpdateSessionRequest,
    current_user: dict = Depends(get_current_user)
):
    """更新 Session"""
    try:
        service = SessionService(SessionLocal)
        result = service.update_session(
            session_id=session_id,
            user_id=current_user["user_id"],
            title=request.title,
            metadata=request.metadata,
            status=request.status
        )
        return result
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"更新 Session 失败: {e!s}"
        )


@router.delete(
    "/{session_id}",
    status_code=status.HTTP_204_NO_CONTENT,
    summary="删除 Session",
    description="删除指定的会话"
)
async def delete_session(
    session_id: str,
    current_user: dict = Depends(get_current_user)
):
    """删除 Session"""
    try:
        service = SessionService(SessionLocal)
        service.delete_session(session_id=session_id, user_id=current_user["user_id"])
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"删除 Session 失败: {e!s}"
        )


@router.post(
    "/{session_id}/close",
    response_model=SessionResponse,
    summary="关闭 Session",
    description="关闭指定的会话"
)
async def close_session(
    session_id: str,
    current_user: dict = Depends(get_current_user)
):
    """关闭 Session"""
    try:
        service = SessionService(SessionLocal)
        result = service.update_session(
            session_id=session_id,
            user_id=current_user["user_id"],
            status="closed"
        )
        # Evict from in-memory cache so closed sessions don't consume RAM.
        try:
            from api.routers.chat import _session_cache
            _session_cache.pop(session_id, None)
        except Exception:
            pass
        return result
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"关闭 Session 失败: {e!s}"
        )
