"""Context Service - 上下文快照管理"""

from datetime import datetime, timezone
from typing import Dict, Any, Optional, List
from sqlalchemy import text
from sqlalchemy.orm import Session
from uuid_utils import uuid7
import json

from api.repositories import SessionRepository, EventRepository
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from core.auth.audit_logger import AuditLogger
# from sqlalchemy.orm import Session
from api.database import get_db_session


class ContextService:
    """Context 业务服务"""
    
    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.session_repo = SessionRepository(db_session)
        self.event_repo = EventRepository(db_session)
        # self.db = next(get_db_session())
        self.audit = AuditLogger(db_session)
    
    def create_snapshot(
        self,
        user_id: str,
        session_id: str,
        event_id: str,
        context_data: Dict[str, Any]
    ) -> Dict[str, Any]:
        """创建上下文快照"""
        # 验证权限
        session = self.session_repo.get_by_id(session_id)
        if not session or session.user_id != user_id:
            raise PermissionDeniedError(f"无权限访问 Session {session_id}")
        
        try:
            context_capture_id = str(uuid7())
            
            # 插入快照 - 使用实际的表字段
            self.db_session.execute(
                text("""
                INSERT INTO context_snapshots
                (context_capture_id, session_id, event_id, system_prompt, skill_definitions,
                 selected_events, code_context, documentation, total_tokens, 
                 token_budget, assembly_time_ms, relevance_scores, task_type, created_at)
                VALUES (:context_capture_id, :session_id, :event_id, :system_prompt, :skill_definitions,
                        :selected_events, :code_context, :documentation, :total_tokens,
                        :token_budget, :assembly_time_ms, :relevance_scores, :task_type, :created_at)
                """),
                {
                    "context_capture_id": context_capture_id,
                    "session_id": session_id,
                    "event_id": event_id,
                    "system_prompt": context_data.get("system_prompt"),
                    "skill_definitions": json.dumps(context_data.get("skill_definitions")),
                    "selected_events": json.dumps(context_data.get("selected_events")),
                    "code_context": json.dumps(context_data.get("code_context")),
                    "documentation": json.dumps(context_data.get("documentation")),
                    "total_tokens": context_data.get("total_tokens"),
                    "token_budget": json.dumps(context_data.get("token_budget")),
                    "assembly_time_ms": context_data.get("assembly_time_ms"),
                    "relevance_scores": json.dumps(context_data.get("relevance_scores")),
                    "task_type": context_data.get("task_type"),
                    "created_at": datetime.now(timezone.utc)
                }
            )
            self.db_session.commit()
            
            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="context_snapshot_create",
                resource_type="context_snapshot",
                resource_id=context_capture_id,
                details={"session_id": session_id, "event_id": event_id},
                status="success"
            )
            
            return {
                "context_capture_id": context_capture_id,
                "session_id": session_id,
                "event_id": event_id,
                "context_data": context_data,
                "created_at": datetime.now(timezone.utc).isoformat()
            }
            
        except Exception as e:
            self.audit.log(
                user_id=user_id,
                action="context_snapshot_create",
                resource_type="context_snapshot",
                resource_id="unknown",
                details={"error": str(e)},
                status="failed"
            )
            raise
    
    def get_snapshot(
        self,
        context_capture_id: str,
        user_id: str
    ) -> Dict[str, Any]:
        """获取上下文快照"""
        try:
            result = self.db_session.execute(
                text("""
                    SELECT cs.context_capture_id, cs.session_id, cs.event_id, 
                           cs.system_prompt, cs.skill_definitions, cs.selected_events,
                           cs.code_context, cs.documentation, cs.total_tokens,
                           cs.token_budget, cs.assembly_time_ms, cs.relevance_scores,
                           cs.task_type, cs.created_at
                    FROM context_snapshots cs
                    JOIN sessions s ON cs.session_id = s.session_id
                    WHERE cs.context_capture_id = :context_capture_id AND s.user_id = :user_id
                    """),
                {"context_capture_id": context_capture_id, "user_id": user_id}
                )
            
            row = result.first()
            
            if not row:
                raise ResourceNotFoundError(f"Snapshot {context_capture_id} 不存在")
            
            result_dict = dict(row._mapping)
            
            # 重构为 context_data
            context_data = {
                "system_prompt": result_dict["system_prompt"],
                "skill_definitions": json.loads(result_dict["skill_definitions"]) if result_dict["skill_definitions"] else [],
                "selected_events": json.loads(result_dict["selected_events"]) if result_dict["selected_events"] else [],
                "code_context": json.loads(result_dict["code_context"]) if result_dict["code_context"] else {},
                "documentation": json.loads(result_dict["documentation"]) if result_dict["documentation"] else {},
                "total_tokens": result_dict["total_tokens"],
                "token_budget": json.loads(result_dict["token_budget"]) if result_dict["token_budget"] else {},
                "assembly_time_ms": result_dict["assembly_time_ms"],
                "relevance_scores": json.loads(result_dict["relevance_scores"]) if result_dict["relevance_scores"] else {},
                "task_type": result_dict["task_type"]
            }
            
            return {
                "context_capture_id": result_dict["context_capture_id"],
                "session_id": result_dict["session_id"],
                "event_id": result_dict["event_id"],
                "context_data": context_data,
                "created_at": result_dict["created_at"].isoformat() if result_dict.get("created_at") else None
            }
        except ResourceNotFoundError:
            raise
        except Exception as e:
            raise ResourceNotFoundError(f"获取快照失败: {str(e)}")
    
    def list_snapshots(
        self,
        user_id: str,
        session_id: Optional[str] = None,
        limit: int = 50,
        offset: int = 0
    ) -> Dict[str, Any]:
        """列出上下文快照"""
        try:
            if session_id:
                # 验证权限
                session = self.session_repo.get_by_id(session_id)
                if not session or session.user_id != user_id:
                    raise PermissionDeniedError(f"无权限访问 Session {session_id}")
                
                result = self.db_session.execute(
                    text("""
                        SELECT context_capture_id, session_id, event_id, created_at
                        FROM context_snapshots
                        WHERE session_id = :session_id
                        ORDER BY created_at DESC
                        LIMIT :limit OFFSET :offset
                        """),
                    {"session_id": session_id, "limit": limit, "offset": offset}
                )
                snapshots = [dict(row._mapping) for row in result]
                
                count_result = self.db_session.execute(
                    text("SELECT COUNT(*) as total FROM context_snapshots WHERE session_id = :session_id"),
                    {"session_id": session_id}
                )
                total = count_result.first()._mapping["total"]
            else:
                result = self.db_session.execute(
                    text("""
                        SELECT cs.context_capture_id, cs.session_id, cs.event_id, cs.created_at
                        FROM context_snapshots cs
                        JOIN sessions s ON cs.session_id = s.session_id
                        WHERE s.user_id = :user_id
                        ORDER BY cs.created_at DESC
                        LIMIT :limit OFFSET :offset
                        """),
                    {"user_id": user_id, "limit": limit, "offset": offset}
                )
                snapshots = [dict(row._mapping) for row in result]
                
                count_result = self.db_session.execute(
                    text("""
                        SELECT COUNT(*) as total
                        FROM context_snapshots cs
                        JOIN sessions s ON cs.session_id = s.session_id
                        WHERE s.user_id = :user_id
                        """),
                    {"user_id": user_id}
                )
                total = count_result.first()._mapping["total"]
            
            return {
                "snapshots": [
                    {
                        "context_capture_id": s["context_capture_id"],
                        "session_id": s["session_id"],
                        "event_id": s["event_id"],
                        "created_at": s["created_at"].isoformat() if s.get("created_at") else None
                    }
                    for s in snapshots
                    ],
                    "total": total,
                    "limit": limit,
                    "offset": offset
                }
        except PermissionDeniedError:
            raise
        except Exception as e:
            print(f"Error in list_snapshots: {e}")  # Debug
            return {"snapshots": [], "total": 0, "limit": limit, "offset": offset}
