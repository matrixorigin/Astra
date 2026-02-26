"""Replay API Router - 会话重放"""


from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.services.exceptions import PermissionDeniedError, ResourceNotFoundError
from api.services.replay_service import ReplayService

router = APIRouter()


# Request/Response Models
class ReplaySessionRequest(BaseModel):
    """重放会话请求"""
    sandbox_name: str | None = None
    mock_mode: bool = True


class ReplayResponse(BaseModel):
    """重放响应"""
    replay_id: str
    session_id: str
    status: str
    events_replayed: int
    sandbox_name: str | None = None
    mock_mode: bool
    created_at: str


class ComparisonResponse(BaseModel):
    """对比响应"""
    session_id: str
    original_event_count: int
    replay_event_count: int
    difference: int
    match: bool
    compared_at: str


# API Endpoints
@router.post(
    "/sessions/{session_id}/replay",
    response_model=ReplayResponse,
    status_code=status.HTTP_201_CREATED,
    summary="重放会话",
    description="在沙箱环境中重放会话，用于测试和验证"
)
async def replay_session(
    session_id: str,
    request: ReplaySessionRequest,
    current_user: dict = Depends(get_current_user)
):
    """重放会话"""
    try:
        service = ReplayService(SessionLocal)
        result = service.replay_session(
            session_id=session_id,
            user_id=current_user["user_id"],
            sandbox_name=request.sandbox_name,
            mock_mode=request.mock_mode
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
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"重放失败: {e!s}"
        )


@router.get(
    "/sessions/{session_id}/replay/compare",
    response_model=ComparisonResponse,
    summary="对比重放结果",
    description="对比原始会话和重放结果的差异"
)
async def compare_replay(
    session_id: str,
    current_user: dict = Depends(get_current_user)
):
    """对比重放结果
    
    注意：这是简化版本，实际需要先执行重放再对比
    """
    try:
        service = ReplayService(SessionLocal)

        # 先执行重放
        replay_result = service.replay_session(
            session_id=session_id,
            user_id=current_user["user_id"],
            mock_mode=True
        )

        # 对比结果
        comparison = service.compare_outputs(
            session_id=session_id,
            user_id=current_user["user_id"],
            replay_result=replay_result["result"]
        )

        return comparison
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
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"对比失败: {e!s}"
        )
