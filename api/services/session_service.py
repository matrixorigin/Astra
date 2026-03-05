"""Session Service - 业务逻辑层"""

import logging
from datetime import datetime, timezone
from typing import Any

from sqlalchemy.orm import Session

from api.repositories import SessionRepository
from core.auth.audit_logger import AuditLogger
from core.auth.permission_checker import PermissionChecker
from core.db_consumer import DbFactory


class SessionService:
    """Session 业务服务"""

    _logger = logging.getLogger(__name__)

    def __init__(self, db_factory: DbFactory):
        self._db_factory = db_factory
        self.session_repo = SessionRepository(db_factory)
        self.audit = AuditLogger(db_factory)
        self.permission = PermissionChecker(db_factory)

    @property
    def db_session(self) -> Session:
        return self._db_factory()

    def create_session(
        self,
        user_id: str,
        agent_id: str | None = None,
        title: str | None = None,
        metadata: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """创建 Session
        
        Args:
            user_id: 用户ID
            agent_id: Agent ID (可选)
            title: 会话标题
            metadata: 元数据
            
        Returns:
            Session信息
        """
        # 设置默认值
        if metadata is None:
            metadata = {}
        if title is None:
            title = f"Session {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M')}"

        try:
            from uuid_utils import uuid7

            session_data = {
                "session_id": str(uuid7()),  # 生成session_id
                "user_id": user_id,
                "agent_id": agent_id,
                "title": title,
                "session_metadata": metadata,  # 使用正确的字段名
                "status": "active",
                "event_count": 0
            }

            session = self.session_repo.create(session_data)

            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="session_create",
                resource_type="session",
                resource_id=session.session_id,
                details={"title": title, "agent_id": agent_id},
                status="success"
            )

            return {
                "session_id": session.session_id,
                "user_id": session.user_id,
                "agent_id": session.agent_id,
                "title": session.title,
                "metadata": session.session_metadata or {},
                "status": session.status,
                "event_count": session.event_count,
                "created_at": session.created_at.isoformat(),
                "updated_at": session.updated_at.isoformat() if session.updated_at else None,
                "ended_at": session.ended_at.isoformat() if session.ended_at else None
            }

        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="session_create",
                resource_type="session",
                resource_id="unknown",
                details={"error": str(e)},
                status="failed"
            )
            raise

    def get_session(self, session_id: str, user_id: str) -> dict[str, Any]:
        """获取 Session 信息
        
        Args:
            session_id: Session ID
            user_id: 用户ID
            
        Returns:
            Session信息
            
        Raises:
            ValueError: Session不存在或无权限
        """
        session = self.session_repo.get_by_id(session_id)

        if not session:
            raise ValueError(f"Session {session_id} 不存在")

        # 权限检查 - 只能访问自己的Session
        if session.user_id != user_id:
            raise ValueError(f"无权限访问 Session {session_id}")

        return {
            "session_id": session.session_id,
            "user_id": session.user_id,
            "agent_id": session.agent_id,
            "title": session.title,
            "metadata": session.session_metadata or {},
            "status": session.status,
            "event_count": session.event_count,
            "created_at": session.created_at.isoformat(),
            "updated_at": session.updated_at.isoformat() if session.updated_at else None,
            "ended_at": session.ended_at.isoformat() if session.ended_at else None
        }

    def list_sessions(
        self,
        user_id: str,
        agent_id: str | None = None,
        status: str | None = None,
        limit: int = 50,
        offset: int = 0
    ) -> dict[str, Any]:
        """列出用户的 Sessions
        
        Args:
            user_id: 用户ID
            agent_id: 过滤Agent ID
            status: 过滤状态
            limit: 限制数量
            offset: 偏移量
            
        Returns:
            Sessions列表和总数
        """
        sessions, total = self.session_repo.list_by_user(
            user_id=user_id,
            agent_id=agent_id,
            status=status,
            limit=limit,
            offset=offset
        )

        return {
            "sessions": [
                {
                    "session_id": session.session_id,
                    "user_id": session.user_id,
                    "agent_id": session.agent_id,
                    "title": session.title,
                    "metadata": session.session_metadata or {},
                    "status": session.status,
                    "event_count": session.event_count,
                    "created_at": session.created_at.isoformat(),
                    "updated_at": session.updated_at.isoformat() if session.updated_at else None,
                    "ended_at": session.ended_at.isoformat() if session.ended_at else None
                }
                for session in sessions
            ],
            "total": total,
            "limit": limit,
            "offset": offset
        }

    def update_session(
        self,
        session_id: str,
        user_id: str,
        title: str | None = None,
        metadata: dict[str, Any] | None = None,
        status: str | None = None
    ) -> dict[str, Any]:
        """更新 Session
        
        Args:
            session_id: Session ID
            user_id: 用户ID
            title: 新标题
            metadata: 新元数据
            status: 新状态
            
        Returns:
            更新后的Session信息
            
        Raises:
            ValueError: Session不存在或无权限
        """
        session = self.session_repo.get_by_id(session_id)

        if not session:
            raise ValueError(f"Session {session_id} 不存在")

        # 权限检查
        if session.user_id != user_id:
            raise ValueError(f"无权限修改 Session {session_id}")

        # 准备更新数据
        update_data = {}
        if title is not None:
            update_data["title"] = title
        if metadata is not None:
            update_data["session_metadata"] = metadata  # 使用正确的字段名
        if status is not None:
            update_data["status"] = status
            if status == "ended":
                update_data["ended_at"] = datetime.now(timezone.utc)
            if status in ("closed", "ended"):
                # Hooks first: quality scoring and knowledge extraction may
                # read session data.  Sandbox cleanup is destructive — run last.
                # Both are best-effort — failures must not block session close.
                try:
                    self._run_close_hooks(session_id, session.user_id)
                except Exception as e:
                    self._logger.warning("Close hooks failed (non-fatal): %s", e)
                self._cleanup_sandbox(session_id)

        if not update_data:
            # 没有更新内容，直接返回当前信息
            return self.get_session(session_id, user_id)

        try:
            updated_session = self.session_repo.update(session_id, update_data)

            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="session_update",
                resource_type="session",
                resource_id=session_id,
                details=update_data,
                status="success"
            )

            return {
                "session_id": updated_session.session_id,
                "user_id": updated_session.user_id,
                "agent_id": updated_session.agent_id,
                "title": updated_session.title,
                "metadata": updated_session.session_metadata or {},
                "status": updated_session.status,
                "event_count": updated_session.event_count,
                "created_at": updated_session.created_at.isoformat(),
                "updated_at": updated_session.updated_at.isoformat() if updated_session.updated_at else None,
                "ended_at": updated_session.ended_at.isoformat() if updated_session.ended_at else None
            }

        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="session_update",
                resource_type="session",
                resource_id=session_id,
                details={"error": str(e)},
                status="failed"
            )
            raise

    def delete_session(self, session_id: str, user_id: str) -> None:
        """删除 Session
        
        Args:
            session_id: Session ID
            user_id: 用户ID
            
        Raises:
            ValueError: Session不存在或无权限
        """
        session = self.session_repo.get_by_id(session_id)

        if not session:
            raise ValueError(f"Session {session_id} 不存在")

        # 权限检查
        if session.user_id != user_id:
            raise ValueError(f"无权限删除 Session {session_id}")

        try:
            self.session_repo.delete(session_id)

            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="session_delete",
                resource_type="session",
                resource_id=session_id,
                details={"title": session.title},
                status="success"
            )

        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="session_delete",
                resource_type="session",
                resource_id=session_id,
                details={"error": str(e)},
                status="failed"
            )
            raise

    def increment_event_count(self, session_id: str, user_id: str) -> None:
        """Atomically increment event count.

        Args:
            session_id: Session ID
            user_id: 用户ID

        Raises:
            ValueError: Session不存在或无权限
        """
        session = self.session_repo.get_by_id(session_id)

        if not session:
            raise ValueError(f"Session {session_id} 不存在")

        if session.user_id != user_id:
            raise ValueError(f"无权限修改 Session {session_id}")

        try:
            from sqlalchemy import text
            self.session_repo.db.execute(
                text(
                    "UPDATE agent_sessions SET event_count = event_count + 1 "
                    "WHERE session_id = :sid"
                ),
                {"sid": session_id},
            )
            self.session_repo.db.commit()
        except Exception:
            pass

    def _cleanup_sandbox(self, session_id: str) -> None:
        """Clean up sandboxes associated with this session."""
        try:
            from sqlalchemy import text

            from core.sandbox import Sandbox
            result = self.db_session.execute(
                text("SELECT sandbox_name FROM infra_sandbox_metadata WHERE session_id = :sid AND status = 'active'"),
                {"sid": session_id},
            )
            names = [row._mapping["sandbox_name"] for row in result]
            if names:
                sandbox = Sandbox(self._db_factory)
                for name in names:
                    try:
                        sandbox.delete(name, force=True)
                    except Exception:
                        pass
        except Exception:
            pass  # Best-effort, Tier 2 will catch any misses

    def _run_close_hooks(self, session_id: str, user_id: str) -> None:
        """Run lifecycle hooks on session close: quality scoring + knowledge extraction."""
        db = self._db_factory()
        try:
            # Session-level quality scoring
            try:
                from core.evaluation.multi_level_scorer import score_session
                score_session(db, session_id)
            except Exception as e:
                self._logger.warning("Session-level scoring failed (non-fatal): %s", e)

            # Knowledge extraction from causal chains
            try:
                from api.models import Event
                from core.events.event_logger import EventLogger
                from skills.knowledge.api import KnowledgeExtractor

                extractor = KnowledgeExtractor(db, event_logger=EventLogger.from_session(db))
                chains = db.query(Event.causal_chain_id).filter(
                    Event.session_id == session_id
                ).distinct().all()
                for (chain_id,) in chains:
                    extractor.extract_from_chain(chain_id, user_id)
            except Exception as e:
                self._logger.warning("Knowledge extraction failed (non-fatal): %s", e)
        finally:
            db.close()
