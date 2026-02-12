"""Skill Service - 技能管理业务逻辑"""

from datetime import datetime, timezone
from typing import Dict, Any, Optional, List
from sqlalchemy.orm import Session
import hashlib
import json

from api.services.exceptions import ResourceNotFoundError
from core.auth.audit_logger import AuditLogger
from sdk import Database


class SkillService:
    """Skill 业务服务"""
    
    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.db = Database()
        self.audit = AuditLogger(self.db)
    
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
        """注册技能"""
        try:
            code_hash = hashlib.sha256(skill_code.encode()).hexdigest()
            
            # 停用旧版本
            self.db.execute(
                "UPDATE skills_registry SET is_active = 0 WHERE skill_name = %s",
                (skill_name,)
            )
            
            # 插入新版本
            self.db.execute(
                """
                INSERT INTO skills_registry
                (skill_id, skill_name, version, description, requirements,
                 code_hash, is_active, status, category, subcategory,
                 triggers, dependencies, priority, cost_estimate, side_effect_category)
                VALUES (%s, %s, %s, %s, %s, %s, 1, 'active', %s, 'default', '[]', '[]', 5, 'medium', 'read')
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
            
            self.audit.log(
                user_id=user_id,
                action="skill_register",
                resource_type="skill",
                resource_id=skill_id,
                details={"skill_name": skill_name, "version": skill_version},
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
        """获取技能信息"""
        try:
            with self.db.get_cursor() as cursor:
                if version:
                    cursor.execute(
                        """
                        SELECT skill_id, skill_name, version, description, requirements, created_at
                        FROM skills_registry
                        WHERE skill_id = %s AND version = %s
                        """,
                        (skill_id, version)
                    )
                else:
                    cursor.execute(
                        """
                        SELECT skill_id, skill_name, version, description, requirements, created_at
                        FROM skills_registry
                        WHERE skill_id = %s AND is_active = 1
                        ORDER BY created_at DESC
                        LIMIT 1
                        """,
                        (skill_id,)
                    )
                
                result = cursor.fetchone()
                
                if not result:
                    raise ResourceNotFoundError(f"Skill {skill_id} 不存在")
                
                return {
                    "skill_id": result["skill_id"],
                    "skill_name": result["skill_name"],
                    "version": result["version"],
                    "description": result["description"],
                    "metadata": json.loads(result["requirements"]) if result["requirements"] else {},
                    "created_at": result["created_at"].isoformat() if result.get("created_at") else None
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
        """列出所有技能"""
        try:
            with self.db.get_cursor() as cursor:
                cursor.execute(
                    """
                    SELECT skill_id, skill_name, version, description
                    FROM skills_registry
                    WHERE is_active = 1
                    ORDER BY created_at DESC
                    LIMIT %s OFFSET %s
                    """,
                    (limit, offset)
                )
                skills = cursor.fetchall()
                
                cursor.execute("SELECT COUNT(*) as total FROM skills_registry WHERE is_active = 1")
                total = cursor.fetchone()["total"]
                
                return {
                    "skills": [
                        {
                            "skill_id": s["skill_id"],
                            "skill_name": s["skill_name"],
                            "version": s["version"],
                            "description": s["description"]
                        }
                        for s in skills
                    ],
                    "total": total,
                    "limit": limit,
                    "offset": offset
                }
        except Exception:
            return {"skills": [], "total": 0, "limit": limit, "offset": offset}
    
    def list_skill_versions(
        self,
        skill_id: str
    ) -> List[Dict[str, Any]]:
        """列出技能的所有版本"""
        try:
            with self.db.get_cursor() as cursor:
                cursor.execute(
                    """
                    SELECT version, description, created_at
                    FROM skills_registry
                    WHERE skill_id = %s
                    ORDER BY created_at DESC
                    """,
                    (skill_id,)
                )
                versions = cursor.fetchall()
                
                return [
                    {
                        "version": v["version"],
                        "description": v["description"],
                        "created_at": v["created_at"].isoformat() if v.get("created_at") else None
                    }
                    for v in versions
                ]
        except Exception:
            return []
