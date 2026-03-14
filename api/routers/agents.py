"""Agent API Router - 使用服务层"""

from typing import Any

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.services.agent_service import AgentService

router = APIRouter()


# Request/Response Models
class CreateAgentRequest(BaseModel):
    """创建 Agent 请求"""

    name: str
    agent_config: dict[str, Any] | None = None
    data_source: dict[str, Any] | None = None


class UpdateAgentRequest(BaseModel):
    """更新 Agent 请求"""

    name: str | None = None
    agent_config: dict[str, Any] | None = None
    data_source: dict[str, Any] | None = None
    is_active: bool | None = None


class AgentResponse(BaseModel):
    """Agent 响应"""

    agent_id: str
    name: str
    agent_type: str
    owner_user_id: str
    agent_config: dict[str, Any]
    data_source: dict[str, Any]
    is_active: bool
    created_at: str
    updated_at: str | None = None


class AgentListResponse(BaseModel):
    """Agent 列表响应"""

    agents: list[AgentResponse]
    total: int


# API Endpoints
@router.post(
    "",
    response_model=AgentResponse,
    status_code=status.HTTP_201_CREATED,
    summary="创建 Agent",
    description="创建一个新的 Agent",
)
async def create_agent(request: CreateAgentRequest, current_user: dict = Depends(get_current_user)):
    """创建 Agent"""
    try:
        service = AgentService(SessionLocal)
        result = service.create_agent(
            user_id=current_user["user_id"],
            name=request.name,
            agent_config=request.agent_config,
            data_source=request.data_source,
        )
        return result
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_400_BAD_REQUEST, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"创建 Agent 失败: {e!s}"
        )


@router.get(
    "",
    response_model=AgentListResponse,
    summary="列出 Agents",
    description="列出当前用户的所有 Agents",
)
async def list_agents(current_user: dict = Depends(get_current_user)):
    """列出 Agents"""
    try:
        service = AgentService(SessionLocal)
        agents = service.list_agents(user_id=current_user["user_id"])
        return {"agents": agents, "total": len(agents)}
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"获取 Agents 失败: {e!s}"
        )


@router.get(
    "/{agent_id}",
    response_model=AgentResponse,
    summary="获取 Agent",
    description="获取指定 Agent 的详细信息",
)
async def get_agent(agent_id: str, current_user: dict = Depends(get_current_user)):
    """获取 Agent"""
    try:
        service = AgentService(SessionLocal)
        result = service.get_agent(agent_id=agent_id, user_id=current_user["user_id"])
        return result
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"获取 Agent 失败: {e!s}"
        )


@router.put(
    "/{agent_id}",
    response_model=AgentResponse,
    summary="更新 Agent",
    description="更新指定 Agent 的信息",
)
async def update_agent(
    agent_id: str, request: UpdateAgentRequest, current_user: dict = Depends(get_current_user)
):
    """更新 Agent"""
    try:
        service = AgentService(SessionLocal)
        result = service.update_agent(
            agent_id=agent_id,
            user_id=current_user["user_id"],
            name=request.name,
            agent_config=request.agent_config,
            data_source=request.data_source,
            is_active=request.is_active,
        )
        return result
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"更新 Agent 失败: {e!s}"
        )


@router.delete(
    "/{agent_id}",
    status_code=status.HTTP_204_NO_CONTENT,
    summary="删除 Agent",
    description="删除指定的 Agent",
)
async def delete_agent(agent_id: str, current_user: dict = Depends(get_current_user)):
    """删除 Agent"""
    try:
        service = AgentService(SessionLocal)
        service.delete_agent(agent_id=agent_id, user_id=current_user["user_id"])
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR, detail=f"删除 Agent 失败: {e!s}"
        )
