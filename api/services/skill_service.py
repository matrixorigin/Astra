"""Skill Service - 技能管理业务逻辑"""

from datetime import datetime, timezone
from typing import Dict, Any, Optional, List
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.repositories import SessionRepository
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from core.auth.audit_logger import AuditLogger
from core.skills import SkillRegistry
from sdk import Database


class SkillService:
    """Skill 业务服务"""
    
    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.db = Database()
        self.audit = AuditLogger(self.db)
        self.registry = SkillRegistry(self.db)
    
    def register_skill(
        self,
        user_id: str,
        skill_id: str,
        skill_name: str,
        skill_version: str,
        skill_code: str,
        description: Optional[str] = None,
        metadata: Optional[Dict[str, Any]] = None
    ) -> Dict[str, Any]:
        """注册技能
        
        Args:
            user_id: 用户ID
            skill_id: 技能ID
            skill_name: 技能名称
            skill_version: 技能版本
            skill_code: 技能代码
            description: 描述
            metadata: 元数据
            
        Returns:
            技能信息
        """
        try:
            import hashlib
            import json
            
            # 计算代码哈希
            code_hash = hashlib.sha256(skill_code.encode()).hexdigest()
            
            # 直接插入数据库
            self.db.execute(
                """
                INSERT INTO skills_registry
                (skill_id, skill_name, version, description, requirements,
                 code_hash, is_active, status, category, subcategory,
                 triggers, dependencies, priority, cost_estimate, side_effect_category)
                VALUES (%s, %s, %s, %s, %s, %s, 1, 'active', %s, 'default', '[]', '[]', 5, 'medium', 'read')
                ON DUPLICATE KEY UPDATE
                    description = VALUES(description),
                    code_hash = VALUES(code_hash),
                    is_active = VALUES(is_active)
                """,
                (
                    skill_id,
                    skill_name,
                    skill_version,
                    description or "",
                    json.dumps(metadata or {}),
                    code_hash,
                    metadata.get("category", "general") if metadata else "general"
                )
            )
            
            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="skill_register",
                resource_type="skill",
                resource_id=skill_id,
                details={
                    "skill_name": skill_name,
                    "version": skill_version
                },
                status="success"
            )
            
            return {
                "skill_id": skill_id,
                "skill_name": skill_name,
                "version": skill_version,
                "description": description or "",
                "metadata": metadata or {},
                "created_at": datetime.now(timezone.utc).isoformat()
            }
            
        except Exception as e:
            self.audit.log(
                user_id=user_id,
                action="skill_register",
                resource_type="skill",
                resource_id=skill_id,
                details={"error": str(e)},
                status="failed"
            )
            raise
    
    def get_skill(
        self,
        skill_id: str,
        version: Optional[str] = None
    ) -> Dict[str, Any]:
        """获取技能信息
        
        Args:
            skill_id: 技能ID
            version: 版本（可选，默认最新版本）
            
        Returns:
            技能信息
            
        Raises:
            ResourceNotFoundError: 技能不存在
        """
        try:
            # 直接查询数据库
            if version:
                query = """
                    SELECT skill_id, skill_name, version, description, requirements, created_at
                    FROM skills_registry
                    WHERE skill_id = %s AND version = %s
                """
                params = (skill_id, version)
            else:
                query = """
                    SELECT skill_id, skill_name, version, description, requirements, created_at
                    FROM skills_registry
                    WHERE skill_id = %s AND is_active = 1
                    ORDER BY created_at DESC
                    LIMIT 1
                """
                params = (skill_id,)
            
            result = self.db.query(query, params)
            
            if not result:
                raise ResourceNotFoundError(f"Skill {skill_id} 不存在")
            
            skill = result[0]
            import json
            
            return {
                "skill_id": skill["skill_id"],
                "skill_name": skill["skill_name"],
                "version": skill["version"],
                "description": skill["description"],
                "metadata": json.loads(skill["requirements"]) if skill["requirements"] else {},
                "created_at": skill["created_at"].isoformat() if skill.get("created_at") else None
            }
        except ResourceNotFoundError:
            raise
        except Exception as e:
            raise ResourceNotFoundError(f"获取技能失败: {str(e)}")
    
    def list_skills(
        self,
        limit: int = 50,
        offset: int = 0
    ) -> Dict[str, Any]:
        """列出所有技能
        
        Args:
            limit: 限制数量
            offset: 偏移量
            
        Returns:
            技能列表
        """
        try:
            skills = self.registry.list_all()
            
            # 分页
            total = len(skills)
            paginated = skills[offset:offset + limit]
            
            return {
                "skills": [
                    {
                        "skill_id": s.skill_id,
                        "skill_name": s.skill_name,
                        "version": s.version,
                        "description": s.description
                    }
                    for s in paginated
                ],
                "total": total,
                "limit": limit,
                "offset": offset
            }
        except Exception as e:
            return {
                "skills": [],
                "total": 0,
                "limit": limit,
                "offset": offset
            }
    
    def list_skill_versions(
        self,
        skill_id: str
    ) -> List[Dict[str, Any]]:
        """列出技能的所有版本
        
        Args:
            skill_id: 技能ID
            
        Returns:
            版本列表
        """
        try:
            versions = self.registry.list_versions(skill_id)
            
            return [
                {
                    "version": v.version,
                    "description": v.description,
                    "created_at": v.created_at.isoformat() if hasattr(v, 'created_at') else None
                }
                for v in versions
            ]
        except Exception:
            return []
