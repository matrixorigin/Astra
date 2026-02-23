"""Skill Service - 技能管理业务逻辑"""

import hashlib
from typing import Any

from sqlalchemy import desc
from sqlalchemy.orm import Session

from api.models import SkillRegistry
from api.services.exceptions import ResourceNotFoundError
from core.auth.audit_logger import AuditLogger


class SkillService:
    """Skill 业务服务"""

    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.audit = AuditLogger(db_session)

    def register_skill(
        self,
        user_id: str,
        skill_id: str,
        skill_name: str,
        skill_version: str,
        skill_code: str,
        description: str | None = None,
        metadata: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """注册技能"""
        try:
            code_hash = hashlib.sha256(skill_code.encode()).hexdigest()

            # 停用旧版本
            self.db_session.query(SkillRegistry).filter(
                SkillRegistry.skill_name == skill_name
            ).update({"is_active": 0})

            # 插入新版本
            # Map requirements/metadata to skill_definition
            skill_definition = metadata or {}

            new_skill = SkillRegistry(
                skill_id=skill_id,
                skill_name=skill_name,
                version=skill_version,
                description=description or "",
                skill_definition=skill_definition,
                code_hash=code_hash,
                is_active=1,
                # status field does not exist in model
                category=skill_definition.get("category", "general"),
                subcategory="default",
                triggers=[],
                dependencies=[],
                priority=5,
                cost_estimate="medium",
                side_effect_profile={"category": "read"}
            )

            self.db_session.add(new_skill)
            self.db_session.commit()
            self.db_session.refresh(new_skill)

            self.audit.log(
                user_id=user_id,
                action="skill_register",
                resource_type="skill",
                resource_id=skill_id,
                details={"skill_name": skill_name, "version": skill_version},
                status="success"
            )

            return {
                "skill_id": new_skill.skill_id,
                "skill_name": new_skill.skill_name,
                "version": new_skill.version,
                "description": new_skill.description,
                "metadata": new_skill.skill_definition or {},
                "created_at": new_skill.created_at.isoformat() if new_skill.created_at else None
            }

        except Exception as e:
            self.db_session.rollback()
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
        version: str | None = None
    ) -> dict[str, Any]:
        """获取技能信息"""
        try:
            query = self.db_session.query(SkillRegistry).filter(SkillRegistry.skill_id == skill_id)

            if version:
                query = query.filter(SkillRegistry.version == version)
            else:
                query = query.filter(SkillRegistry.is_active == 1).order_by(desc(SkillRegistry.created_at))

            skill = query.first()

            if not skill:
                raise ResourceNotFoundError(f"Skill {skill_id} 不存在")

            return {
                "skill_id": skill.skill_id,
                "skill_name": skill.skill_name,
                "version": skill.version,
                "description": skill.description,
                "metadata": skill.skill_definition or {},
                "created_at": skill.created_at.isoformat() if skill.created_at else None
            }
        except ResourceNotFoundError:
            raise
        except Exception as e:
            raise ResourceNotFoundError(f"获取技能失败: {e!s}")

    def list_skills(
        self,
        limit: int = 50,
        offset: int = 0
    ) -> dict[str, Any]:
        """列出所有技能"""
        try:
            total = self.db_session.query(SkillRegistry).filter(SkillRegistry.is_active == 1).count()

            skills = self.db_session.query(SkillRegistry).filter(
                SkillRegistry.is_active == 1
            ).order_by(desc(SkillRegistry.created_at)).offset(offset).limit(limit).all()

            return {
                "skills": [
                    {
                        "skill_id": s.skill_id,
                        "skill_name": s.skill_name,
                        "version": s.version,
                        "description": s.description
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
    ) -> list[dict[str, Any]]:
        """列出技能的所有版本"""
        try:
            # First find the skill name from the ID
            skill = self.db_session.query(SkillRegistry).filter(SkillRegistry.skill_id == skill_id).first()
            if not skill:
                # Fallback: try to match by ID only (though ID usually includes name)
                versions = self.db_session.query(SkillRegistry).filter(
                    SkillRegistry.skill_id == skill_id
                ).order_by(desc(SkillRegistry.version)).all()
            else:
                versions = self.db_session.query(SkillRegistry).filter(
                    SkillRegistry.skill_name == skill.skill_name
                ).order_by(desc(SkillRegistry.version)).all()

            return [
                {
                    "version": v.version,
                    "description": v.description,
                    "created_at": v.created_at.isoformat() if v.created_at else None
                }
                for v in versions
            ]
        except Exception:
            return []
