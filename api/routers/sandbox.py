"""Sandbox API Router

提供 Sandbox 管理的 REST API endpoints
"""

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel, Field

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.services.sandbox_service import SandboxService

router = APIRouter(prefix="/sandbox", tags=["sandbox"])


# Request/Response Models
class CreateSandboxRequest(BaseModel):
    """创建 Sandbox 请求"""

    name: str = Field(..., description="Sandbox 名称", min_length=1, max_length=64)
    description: str = Field("", description="Sandbox 描述", max_length=255)


class SandboxResponse(BaseModel):
    """Sandbox 响应"""

    sandbox_name: str
    description: str = ""
    created_by: str = ""
    created_at: str


class SandboxListResponse(BaseModel):
    """Sandbox 列表响应"""

    sandboxes: list[dict]
    total: int


# API Endpoints
@router.post(
    "",
    response_model=SandboxResponse,
    status_code=status.HTTP_201_CREATED,
    summary="创建 Sandbox",
    description="创建一个新的 sandbox 用于隔离实验",
)
async def create_sandbox(
    request: CreateSandboxRequest, current_user: dict = Depends(get_current_user)
):
    """创建 Sandbox

    需要权限: mo_agent_user
    """
    try:
        service = SandboxService(SessionLocal)
        result = service.create_sandbox(
            name=request.name,
            user_id=current_user["user_id"],
            description=request.description,
            created_by=current_user["user_id"],
        )
        return result
    except PermissionError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_400_BAD_REQUEST, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"创建 sandbox 失败: {e!s}"
        )


@router.get(
    "",
    response_model=SandboxListResponse,
    summary="列出 Sandboxes",
    description="列出当前用户可访问的所有 sandboxes",
)
async def list_sandboxes(pattern: str | None = "%", current_user: dict = Depends(get_current_user)):
    """列出 Sandboxes

    需要权限: mo_agent_user

    Args:
        pattern: 过滤模式 (SQL LIKE pattern)
    """
    try:
        service = SandboxService(SessionLocal)
        sandboxes = service.list_sandboxes(user_id=current_user["user_id"], pattern=pattern)
        return {"sandboxes": sandboxes, "total": len(sandboxes)}
    except PermissionError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"列出 sandboxes 失败: {e!s}"
        )


@router.get(
    "/{name}",
    response_model=SandboxResponse,
    summary="获取 Sandbox 信息",
    description="获取指定 sandbox 的详细信息",
)
async def get_sandbox(name: str, current_user: dict = Depends(get_current_user)):
    """获取 Sandbox 信息

    需要权限: mo_agent_user (且是创建者或 admin)
    """
    try:
        service = SandboxService(SessionLocal)
        return service.get_sandbox_info(name, current_user["user_id"])
    except PermissionError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"获取 sandbox 信息失败: {e!s}",
        )


@router.delete(
    "/{name}",
    status_code=status.HTTP_204_NO_CONTENT,
    summary="删除 Sandbox",
    description="删除指定的 sandbox",
)
async def delete_sandbox(name: str, current_user: dict = Depends(get_current_user)):
    """删除 Sandbox

    需要权限: mo_agent_user (且是创建者或 admin)
    """
    try:
        service = SandboxService(SessionLocal)
        service.delete_sandbox(name, current_user["user_id"])
        return None
    except PermissionError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"删除 sandbox 失败: {e!s}"
        )
