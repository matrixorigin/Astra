"""Event API Router - 使用服务层"""

from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query, status
from pydantic import BaseModel
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.services.event_service import EventService
from api.services.exceptions import PermissionDeniedError, ResourceNotFoundError

router = APIRouter()


# Request/Response Models
class CreateEventRequest(BaseModel):
    """创建 Event 请求"""
    session_id: str
    event_type: str
    content: str
    agent_id: str | None = None
    agent_version: str | None = None
    parent_event_id: str | None = None
    causal_chain_id: str | None = None
    metadata: dict[str, Any] | None = None


class EventResponse(BaseModel):
    """Event 响应"""
    event_id: str
    user_id: str
    session_id: str
    event_type: str
    content: str
    agent_id: str | None = None
    agent_version: str | None = None
    parent_event_id: str | None = None
    causal_chain_id: str
    metadata: dict[str, Any]
    created_at: str


class EventListResponse(BaseModel):
    """Event 列表响应"""
    events: list[EventResponse]
    total: int
    limit: int
    offset: int


# API Endpoints
@router.post(
    "",
    response_model=EventResponse,
    status_code=status.HTTP_201_CREATED,
    summary="创建 Event",
    description="创建一个新的事件"
)
async def create_event(
    request: CreateEventRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """创建 Event"""
    try:
        service = EventService(lambda: db)
        result = service.create_event(
            user_id=current_user["user_id"],
            session_id=request.session_id,
            event_type=request.event_type,
            content=request.content,
            agent_id=request.agent_id,
            agent_version=request.agent_version,
            parent_event_id=request.parent_event_id,
            causal_chain_id=request.causal_chain_id,
            metadata=request.metadata
        )
        return result
    except ResourceNotFoundError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except PermissionDeniedError as e:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail=str(e)
        )
    except ResourceNotFoundError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except PermissionDeniedError as e:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail=str(e)
        )
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"创建 Event 失败: {e!s}"
        )


@router.get(
    "",
    response_model=EventListResponse,
    summary="列出 Events",
    description="列出当前用户的事件"
)
async def list_events(
    session_id: str | None = Query(None, description="过滤Session ID"),
    event_type: str | None = Query(None, description="过滤事件类型"),
    agent_id: str | None = Query(None, description="过滤Agent ID"),
    causal_chain_id: str | None = Query(None, description="过滤因果链ID"),
    limit: int = Query(50, ge=1, le=100, description="限制数量"),
    offset: int = Query(0, ge=0, description="偏移量"),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """列出 Events"""
    try:
        service = EventService(lambda: db)
        result = service.list_events(
            user_id=current_user["user_id"],
            session_id=session_id,
            event_type=event_type,
            agent_id=agent_id,
            causal_chain_id=causal_chain_id,
            limit=limit,
            offset=offset
        )
        return result
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"获取 Events 失败: {e!s}"
        )


@router.get(
    "/{event_id}",
    response_model=EventResponse,
    summary="获取 Event",
    description="获取指定事件的详细信息"
)
async def get_event(
    event_id: str,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """获取 Event"""
    try:
        service = EventService(lambda: db)
        result = service.get_event(event_id=event_id, user_id=current_user["user_id"])
        return result
    except ResourceNotFoundError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except PermissionDeniedError as e:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail=str(e)
        )
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"获取 Event 失败: {e!s}"
        )


@router.get(
    "/causal-chain/{causal_chain_id}",
    response_model=list[EventResponse],
    summary="获取因果链",
    description="获取因果链中的所有事件"
)
async def get_causal_chain(
    causal_chain_id: str,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """获取因果链"""
    try:
        service = EventService(lambda: db)
        result = service.get_causal_chain(
            causal_chain_id=causal_chain_id,
            user_id=current_user["user_id"]
        )
        return result
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"获取因果链失败: {e!s}"
        )


@router.get(
    "/session/{session_id}",
    response_model=EventListResponse,
    summary="获取Session事件",
    description="获取指定会话中的所有事件"
)
async def get_session_events(
    session_id: str,
    limit: int = Query(100, ge=1, le=500, description="限制数量"),
    offset: int = Query(0, ge=0, description="偏移量"),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """获取Session事件"""
    try:
        service = EventService(lambda: db)
        result = service.get_session_events(
            session_id=session_id,
            user_id=current_user["user_id"],
            limit=limit,
            offset=offset
        )
        return result
    except ResourceNotFoundError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except PermissionDeniedError as e:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail=str(e)
        )
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"获取Session事件失败: {e!s}"
        )


@router.delete(
    "/{event_id}",
    status_code=status.HTTP_204_NO_CONTENT,
    summary="删除 Event",
    description="删除指定的事件"
)
async def delete_event(
    event_id: str,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """删除 Event"""
    try:
        service = EventService(lambda: db)
        service.delete_event(event_id=event_id, user_id=current_user["user_id"])
    except ResourceNotFoundError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except PermissionDeniedError as e:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail=str(e)
        )
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=str(e)
        )
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"删除 Event 失败: {e!s}"
        )
