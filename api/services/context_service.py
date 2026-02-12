"""Context Service - 上下文快照管理"""

from datetime import datetime, timezone
from typing import Dict, Any, Optional, List
from sqlalchemy.orm import Session
from uuid_utils import uuid7
import json

from api.repositories import SessionRepository, EventRepository
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from core.auth.audit_logger import AuditLogger
from sdk import Database


class ContextService:
    """Context 业务服务"""
    
    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.session_repo = SessionRepository(db_session)
        self.event_repo = EventRepository(db_session)
        self.db = Database()
        self.audit = AuditLogger(self.db)
    
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
            snapshot_id = str(uuid7())
            
            # 插入快照 - 使用实际的表字段
            self.db.execute(
                """
                INSERT INTO context_snapshots
                (snapshot_id, session_id, event_id, system_prompt, skill_definitions,
                 selected_events, code_context, documentation, total_tokens, 
                 token_budget, assembly_time_ms, relevance_scores, task_type, created_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
                """,
                (
                    snapshot_id,
                    session_id,
                    event_id,
                    context_data.get("system_prompt"),
                    json.dumps(context_data.get("skill_definitions", [])),
                    json.dumps(context_data.get("selected_events", [])),
                    json.dumps(context_data.get("code_context", {})),
                    json.dumps(context_data.get("documentation", {})),
                    context_data.get("total_tokens"),
                    json.dumps(context_data.get("token_budget", {})),
                    context_data.get("assembly_time_ms"),
                    json.dumps(context_data.get("relevance_scores", {})),
                    context_data.get("task_type"),
                    datetime.now(timezone.utc)
                )
            )
            
            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="context_snapshot_create",
                resource_type="context_snapshot",
                resource_id=snapshot_id,
                details={"session_id": session_id, "event_id": event_id},
                status="success"
            )
            
            return {
                "snapshot_id": snapshot_id,
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
        snapshot_id: str,
        user_id: str
    ) -> Dict[str, Any]:
        """获取上下文快照"""
        try:
            with self.db.get_cursor() as cursor:
                cursor.execute(
                    """
                    SELECT cs.snapshot_id, cs.session_id, cs.event_id, 
                           cs.system_prompt, cs.skill_definitions, cs.selected_events,
                           cs.code_context, cs.documentation, cs.total_tokens,
                           cs.token_budget, cs.assembly_time_ms, cs.relevance_scores,
                           cs.task_type, cs.created_at
                    FROM context_snapshots cs
                    JOIN sessions s ON cs.session_id = s.session_id
                    WHERE cs.snapshot_id = %s AND s.user_id = %s
                    """,
                    (snapshot_id, user_id)
                )
                
                result = cursor.fetchone()
                
                if not result:
                    raise ResourceNotFoundError(f"Snapshot {snapshot_id} 不存在")
                
                # 重构为 context_data
                context_data = {
                    "system_prompt": result["system_prompt"],
                    "skill_definitions": json.loads(result["skill_definitions"]) if result["skill_definitions"] else [],
                    "selected_events": json.loads(result["selected_events"]) if result["selected_events"] else [],
                    "code_context": json.loads(result["code_context"]) if result["code_context"] else {},
                    "documentation": json.loads(result["documentation"]) if result["documentation"] else {},
                    "total_tokens": result["total_tokens"],
                    "token_budget": json.loads(result["token_budget"]) if result["token_budget"] else {},
                    "assembly_time_ms": result["assembly_time_ms"],
                    "relevance_scores": json.loads(result["relevance_scores"]) if result["relevance_scores"] else {},
                    "task_type": result["task_type"]
                }
                
                return {
                    "snapshot_id": result["snapshot_id"],
                    "session_id": result["session_id"],
                    "event_id": result["event_id"],
                    "context_data": context_data,
                    "created_at": result["created_at"].isoformat() if result.get("created_at") else None
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
            with self.db.get_cursor() as cursor:
                if session_id:
                    # 验证权限
                    session = self.session_repo.get_by_id(session_id)
                    if not session or session.user_id != user_id:
                        raise PermissionDeniedError(f"无权限访问 Session {session_id}")
                    
                    cursor.execute(
                        """
                        SELECT snapshot_id, session_id, event_id, created_at
                        FROM context_snapshots
                        WHERE session_id = %s
                        ORDER BY created_at DESC
                        LIMIT %s OFFSET %s
                        """,
                        (session_id, limit, offset)
                    )
                    snapshots = cursor.fetchall()
                    
                    cursor.execute(
                        "SELECT COUNT(*) as total FROM context_snapshots WHERE session_id = %s",
                        (session_id,)
                    )
                    total = cursor.fetchone()["total"]
                else:
                    cursor.execute(
                        """
                        SELECT cs.snapshot_id, cs.session_id, cs.event_id, cs.created_at
                        FROM context_snapshots cs
                        JOIN sessions s ON cs.session_id = s.session_id
                        WHERE s.user_id = %s
                        ORDER BY cs.created_at DESC
                        LIMIT %s OFFSET %s
                        """,
                        (user_id, limit, offset)
                    )
                    snapshots = cursor.fetchall()
                    
                    cursor.execute(
                        """
                        SELECT COUNT(*) as total
                        FROM context_snapshots cs
                        JOIN sessions s ON cs.session_id = s.session_id
                        WHERE s.user_id = %s
                        """,
                        (user_id,)
                    )
                    total = cursor.fetchone()["total"]
                
                return {
                    "snapshots": [
                        {
                            "snapshot_id": s["snapshot_id"],
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
