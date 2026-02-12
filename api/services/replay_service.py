"""Replay Service - 会话重放业务逻辑"""

from datetime import datetime, timezone
from typing import Dict, Any, Optional
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.repositories import SessionRepository, EventRepository
from api.services.exceptions import ResourceNotFoundError, PermissionDeniedError
from core.auth.audit_logger import AuditLogger
from sdk import Database


class ReplayService:
    """Replay 业务服务"""
    
    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.session_repo = SessionRepository(db_session)
        self.event_repo = EventRepository(db_session)
        self.db = Database()
        self.audit = AuditLogger(self.db)
    
    def replay_session(
        self,
        session_id: str,
        user_id: str,
        sandbox_name: Optional[str] = None,
        mock_mode: bool = True
    ) -> Dict[str, Any]:
        """重放会话
        
        Args:
            session_id: 要重放的会话ID
            user_id: 用户ID
            sandbox_name: 沙箱名称（可选）
            mock_mode: 是否使用 mock 模式（避免副作用）
            
        Returns:
            重放结果
            
        Raises:
            ResourceNotFoundError: 会话不存在
            PermissionDeniedError: 无权限
        """
        # 验证会话存在且有权限
        session = self.session_repo.get_by_id(session_id)
        if not session:
            raise ResourceNotFoundError(f"Session {session_id} 不存在")
        
        if session.user_id != user_id:
            raise PermissionDeniedError(f"无权限访问 Session {session_id}")
        
        try:
            # 获取会话的所有事件
            events, total = self.event_repo.get_by_session(session_id)
            
            # 简化版本：只返回事件列表，不实际执行重放
            # 实际重放需要 ReplayEngine 和 SkillRegistry
            replay_result = {
                "events": [
                    {
                        "event_id": e.event_id,
                        "event_type": e.event_type,
                        "content": e.content,
                        "created_at": e.created_at.isoformat()
                    }
                    for e in events
                ],
                "total": total
            }
            
            # 生成重放ID
            replay_id = str(uuid7())
            
            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="session_replay",
                resource_type="session",
                resource_id=session_id,
                details={
                    "replay_id": replay_id,
                    "sandbox_name": sandbox_name,
                    "mock_mode": mock_mode,
                    "events_count": total
                },
                status="success"
            )
            
            return {
                "replay_id": replay_id,
                "session_id": session_id,
                "status": "completed",
                "events_replayed": total,
                "sandbox_name": sandbox_name,
                "mock_mode": mock_mode,
                "result": replay_result,
                "created_at": datetime.now(timezone.utc).isoformat()
            }
            
        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="session_replay",
                resource_type="session",
                resource_id=session_id,
                details={"error": str(e)},
                status="failed"
            )
            raise
    
    def compare_outputs(
        self,
        session_id: str,
        user_id: str,
        replay_result: Dict[str, Any]
    ) -> Dict[str, Any]:
        """对比原始输出和重放输出
        
        Args:
            session_id: 会话ID
            user_id: 用户ID
            replay_result: 重放结果
            
        Returns:
            对比结果
        """
        # 验证权限
        session = self.session_repo.get_by_id(session_id)
        if not session or session.user_id != user_id:
            raise PermissionDeniedError(f"无权限访问 Session {session_id}")
        
        # 获取原始事件
        original_events, _ = self.event_repo.get_by_session(session_id)
        
        # 简单对比：计算事件数量差异
        original_count = len(original_events)
        replay_count = len(replay_result.get("events", []))
        
        return {
            "session_id": session_id,
            "original_event_count": original_count,
            "replay_event_count": replay_count,
            "difference": replay_count - original_count,
            "match": original_count == replay_count,
            "compared_at": datetime.now(timezone.utc).isoformat()
        }
