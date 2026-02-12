"""Decision Service - 决策审计管理"""

from datetime import datetime, timezone
from typing import Dict, Any, Optional
from sqlalchemy.orm import Session
from uuid_utils import uuid7
import json

from api.repositories import SessionRepository, EventRepository
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from core.auth.audit_logger import AuditLogger
from sdk import Database


class DecisionService:
    """Decision 业务服务"""
    
    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.session_repo = SessionRepository(db_session)
        self.event_repo = EventRepository(db_session)
        self.db = Database()
        self.audit = AuditLogger(self.db)
    
    def record_decision(
        self,
        user_id: str,
        session_id: str,
        event_id: str,
        snapshot_id: str,
        decision_type: str,
        decision_output: Dict[str, Any],
        model_params: Optional[Dict[str, Any]] = None
    ) -> Dict[str, Any]:
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
            
            # 插入决策记录
            self.db.execute(
                """
                INSERT INTO decision_audit
                (decision_id, session_id, event_id, snapshot_id, decision_type,
                 decision_output, model_params, created_at)
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
                """,
                (
                    decision_id,
                    session_id,
                    event_id,
                    snapshot_id,
                    decision_type,
                    json.dumps(decision_output),
                    json.dumps(model_params or {}),
                    datetime.now(timezone.utc)
                )
            )
            
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
                "decision_id": decision_id,
                "session_id": session_id,
                "event_id": event_id,
                "snapshot_id": snapshot_id,
                "decision_type": decision_type,
                "decision_output": decision_output,
                "model_params": model_params or {},
                "created_at": datetime.now(timezone.utc).isoformat()
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
    ) -> Dict[str, Any]:
        """获取决策记录"""
        try:
            with self.db.get_cursor() as cursor:
                cursor.execute(
                    """
                    SELECT d.decision_id, d.session_id, d.event_id, d.snapshot_id,
                           d.decision_type, d.decision_output, d.model_params, d.created_at
                    FROM decision_audit d
                    JOIN sessions s ON d.session_id = s.session_id
                    WHERE d.decision_id = %s AND s.user_id = %s
                    """,
                    (decision_id, user_id)
                )
                
                result = cursor.fetchone()
                
                if not result:
                    raise ResourceNotFoundError(f"Decision {decision_id} 不存在")
                
                return {
                    "decision_id": result["decision_id"],
                    "session_id": result["session_id"],
                    "event_id": result["event_id"],
                    "snapshot_id": result["snapshot_id"],
                    "decision_type": result["decision_type"],
                    "decision_output": json.loads(result["decision_output"]) if result["decision_output"] else {},
                    "model_params": json.loads(result["model_params"]) if result["model_params"] else {},
                    "created_at": result["created_at"].isoformat() if result.get("created_at") else None
                }
        except ResourceNotFoundError:
            raise
        except Exception as e:
            raise ResourceNotFoundError(f"获取决策失败: {str(e)}")
    
    def get_decision_with_context(
        self,
        decision_id: str,
        user_id: str
    ) -> Dict[str, Any]:
        """获取决策及其完整上下文（用于审计）"""
        try:
            # 获取决策
            decision = self.get_decision(decision_id, user_id)
            
            # 获取上下文快照
            with self.db.get_cursor() as cursor:
                cursor.execute(
                    """
                    SELECT snapshot_id, system_prompt, skill_definitions, selected_events,
                           code_context, documentation, total_tokens, token_budget,
                           assembly_time_ms, relevance_scores, task_type
                    FROM context_snapshots
                    WHERE snapshot_id = %s
                    """,
                    (decision["snapshot_id"],)
                )
                
                snapshot = cursor.fetchone()
                
                if snapshot:
                    decision["context"] = {
                        "system_prompt": snapshot["system_prompt"],
                        "skill_definitions": json.loads(snapshot["skill_definitions"]) if snapshot["skill_definitions"] else [],
                        "selected_events": json.loads(snapshot["selected_events"]) if snapshot["selected_events"] else [],
                        "code_context": json.loads(snapshot["code_context"]) if snapshot["code_context"] else {},
                        "documentation": json.loads(snapshot["documentation"]) if snapshot["documentation"] else {},
                        "total_tokens": snapshot["total_tokens"],
                        "token_budget": json.loads(snapshot["token_budget"]) if snapshot["token_budget"] else {},
                        "assembly_time_ms": snapshot["assembly_time_ms"],
                        "relevance_scores": json.loads(snapshot["relevance_scores"]) if snapshot["relevance_scores"] else {},
                        "task_type": snapshot["task_type"]
                    }
                
                return decision
                
        except ResourceNotFoundError:
            raise
        except Exception as e:
            raise ResourceNotFoundError(f"获取决策上下文失败: {str(e)}")
    
    def list_decisions(
        self,
        user_id: str,
        session_id: Optional[str] = None,
        decision_type: Optional[str] = None,
        limit: int = 50,
        offset: int = 0
    ) -> Dict[str, Any]:
        """列出决策记录"""
        try:
            with self.db.get_cursor() as cursor:
                # 构建查询
                where_clauses = ["s.user_id = %s"]
                params = [user_id]
                
                if session_id:
                    where_clauses.append("d.session_id = %s")
                    params.append(session_id)
                
                if decision_type:
                    where_clauses.append("d.decision_type = %s")
                    params.append(decision_type)
                
                where_sql = " AND ".join(where_clauses)
                
                # 查询决策
                cursor.execute(
                    f"""
                    SELECT d.decision_id, d.session_id, d.event_id, d.snapshot_id,
                           d.decision_type, d.created_at
                    FROM decision_audit d
                    JOIN sessions s ON d.session_id = s.session_id
                    WHERE {where_sql}
                    ORDER BY d.created_at DESC
                    LIMIT %s OFFSET %s
                    """,
                    params + [limit, offset]
                )
                decisions = cursor.fetchall()
                
                # 查询总数
                cursor.execute(
                    f"""
                    SELECT COUNT(*) as total
                    FROM decision_audit d
                    JOIN sessions s ON d.session_id = s.session_id
                    WHERE {where_sql}
                    """,
                    params
                )
                total = cursor.fetchone()["total"]
                
                return {
                    "decisions": [
                        {
                            "decision_id": d["decision_id"],
                            "session_id": d["session_id"],
                            "event_id": d["event_id"],
                            "snapshot_id": d["snapshot_id"],
                            "decision_type": d["decision_type"],
                            "created_at": d["created_at"].isoformat() if d.get("created_at") else None
                        }
                        for d in decisions
                    ],
                    "total": total,
                    "limit": limit,
                    "offset": offset
                }
        except Exception as e:
            print(f"Error in list_decisions: {e}")
            return {"decisions": [], "total": 0, "limit": limit, "offset": offset}
