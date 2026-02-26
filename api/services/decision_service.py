"""Decision Service - 决策审计管理"""

from typing import Any

from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.repositories import DecisionRepository, EventRepository, SessionRepository
from api.services.exceptions import PermissionDeniedError, ResourceNotFoundError
from core.auth.audit_logger import AuditLogger
from core.db_consumer import DbFactory


class DecisionService:
    """Decision 业务服务"""

    def __init__(self, db_factory: DbFactory):
        self._db_factory = db_factory
        self.decision_repo = DecisionRepository(db_factory)
        self.session_repo = SessionRepository(db_factory)
        self.event_repo = EventRepository(db_factory)
        self.audit = AuditLogger(db_factory)

    @property
    def db_session(self) -> Session:
        return self._db_factory()

    def record_decision(
        self,
        user_id: str,
        session_id: str,
        event_id: str,
        context_capture_id: str,
        decision_type: str,
        decision_output: dict[str, Any],
        model_params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """记录决策
        
        Args:
            user_id: 用户ID
            session_id: 会话ID
            event_id: 事件ID
            snapshot_id: 上下文快照ID
            decision_type: 决策类型（skill_selection, response_generation, etc.）
            decision_output: 决策输出
            model_params: 模型参数（model, temperature, etc.）
            
        Returns:
            决策记录
        """
        # 验证权限
        session = self.session_repo.get_by_id(session_id)
        if not session or session.user_id != user_id:
            raise PermissionDeniedError(f"无权限访问 Session {session_id}")

        try:
            decision_id = str(uuid7())

            # 使用 ORM 插入决策记录
            decision_data = {
                "decision_id": decision_id,
                "session_id": session_id,
                "event_id": event_id,
                "context_capture_id": context_capture_id,
                "decision_type": decision_type,
                "decision_output": decision_output,
                "model_params": model_params or {}
            }

            decision = self.decision_repo.create(decision_data)

            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="decision_record",
                resource_type="decision",
                resource_id=decision_id,
                details={
                    "session_id": session_id,
                    "event_id": event_id,
                    "decision_type": decision_type
                },
                status="success"
            )

            return {
                "decision_id": decision.decision_id,
                "session_id": decision.session_id,
                "event_id": decision.event_id,
                "context_capture_id": decision.context_capture_id,
                "decision_type": decision.decision_type,
                "decision_output": decision.decision_output,
                "model_params": decision.model_params,
                "created_at": decision.created_at.isoformat()
            }

        except Exception as e:
            self.audit.log(
                user_id=user_id,
                action="decision_record",
                resource_type="decision",
                resource_id="unknown",
                details={"error": str(e)},
                status="failed"
            )
            raise

    def get_decision(
        self,
        decision_id: str,
        user_id: str
    ) -> dict[str, Any]:
        """获取决策记录"""
        decision = self.decision_repo.get_by_id_with_user(decision_id, user_id)

        if not decision:
            raise ResourceNotFoundError(f"Decision {decision_id} 不存在")

        return {
            "decision_id": decision.decision_id,
            "session_id": decision.session_id,
            "event_id": decision.event_id,
            "context_capture_id": decision.context_capture_id,
            "decision_type": decision.decision_type,
            "decision_output": decision.decision_output,
            "model_params": decision.model_params,
            "created_at": decision.created_at.isoformat()
        }

    def get_decision_with_context(
        self,
        decision_id: str,
        user_id: str
    ) -> dict[str, Any]:
        """获取决策及其完整上下文（用于审计）"""
        from api.models import ContextSnapshot

        # 获取决策
        decision = self.get_decision(decision_id, user_id)

        # 使用 ORM 获取上下文快照
        snapshot = self.db_session.query(ContextSnapshot).filter(
            ContextSnapshot.context_capture_id == decision["context_capture_id"]
        ).first()

        if snapshot:
            decision["context"] = {
                "system_prompt": snapshot.system_prompt,
                "skill_definitions": snapshot.skill_definitions,
                "selected_events": snapshot.selected_events,
                "code_context": snapshot.code_context,
                "documentation": snapshot.documentation,
            }

        return decision

    def list_decisions(
        self,
        user_id: str,
        session_id: str | None = None,
        decision_type: str | None = None,
        limit: int = 50,
        offset: int = 0
    ) -> dict[str, Any]:
        """列出决策记录"""
        if session_id:
            # 验证权限
            session = self.session_repo.get_by_id(session_id)
            if not session or session.user_id != user_id:
                raise PermissionDeniedError(f"无权限访问 Session {session_id}")

            decisions, total = self.decision_repo.list_by_session(
                session_id=session_id,
                limit=limit,
                offset=offset
            )
        else:
            decisions, total = self.decision_repo.list_by_user(
                user_id=user_id,
                decision_type=decision_type,
                limit=limit,
                offset=offset
            )

        return {
            "decisions": [
                {
                    "decision_id": d.decision_id,
                    "session_id": d.session_id,
                    "event_id": d.event_id,
                    "context_capture_id": d.context_capture_id,
                    "decision_type": d.decision_type,
                    "created_at": d.created_at.isoformat()
                }
                for d in decisions
            ],
            "total": total,
            "limit": limit,
            "offset": offset
        }
