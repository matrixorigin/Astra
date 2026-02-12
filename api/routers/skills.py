"""Skill API Router - 技能管理"""

from typing import Dict, Any, Optional, List
from fastapi import APIRouter, Depends, HTTPException, status, Query
from pydantic import BaseModel
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.services.skill_service import SkillService
from api.services.exceptions import ResourceNotFoundError


router = APIRouter()


class RegisterSkillRequest(BaseModel):
    """注册技能请求"""
    skill_id: str
    skill_name: str
    skill_version: str
    skill_code: str
    description: Optional[str] = None
    metadata: Optional[Dict[str, Any]] = None


class SkillResponse(BaseModel):
    """技能响应"""
    skill_id: str
    skill_name: str
    version: str
    description: str
    metadata: Dict[str, Any]
    created_at: Optional[str] = None


class SkillListResponse(BaseModel):
    """技能列表响应"""
    skills: List[Dict[str, Any]]
    total: int
    limit: int
    offset: int


class SkillVersionResponse(BaseModel):
    """技能版本响应"""
    version: str
    description: str
    created_at: Optional[str] = None


@router.post(
    "",
    response_model=SkillResponse,
    status_code=status.HTTP_201_CREATED,
    summary="注册技能"
)
async def register_skill(
    request: RegisterSkillRequest,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """注册技能"""
    try:
        service = SkillService(db)
        result = service.register_skill(
            user_id=current_user["user_id"],
            skill_id=request.skill_id,
            skill_name=request.skill_name,
            skill_version=request.skill_version,
            skill_code=request.skill_code,
            description=request.description,
            metadata=request.metadata
        )
        return result
    except Exception as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"注册技能失败: {str(e)}"
        )


@router.get(
    "",
    response_model=SkillListResponse,
    summary="列出技能"
)
async def list_skills(
    limit: int = Query(50, ge=1, le=100),
    offset: int = Query(0, ge=0),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """列出技能"""
    service = SkillService(db)
    return service.list_skills(limit=limit, offset=offset)


@router.get(
    "/{skill_id}",
    response_model=SkillResponse,
    summary="获取技能"
)
async def get_skill(
    skill_id: str,
    version: Optional[str] = Query(None),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """获取技能"""
    try:
        service = SkillService(db)
        return service.get_skill(skill_id=skill_id, version=version)
    except ResourceNotFoundError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))


@router.get(
    "/{skill_id}/versions",
    response_model=List[SkillVersionResponse],
    summary="列出技能版本"
)
async def list_skill_versions(
    skill_id: str,
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user)
):
    """列出技能版本"""
    service = SkillService(db)
    return service.list_skill_versions(skill_id=skill_id)
