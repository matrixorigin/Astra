"""Event Service - 业务逻辑层"""

from typing import Any

from sqlalchemy.orm import Session

from api.repositories import EventRepository, SessionRepository
from api.services.exceptions import PermissionDeniedError, ResourceNotFoundError
from core.auth.audit_logger import AuditLogger
from core.auth.permission_checker import PermissionChecker
from core.db_consumer import DbFactory


class EventService:
    """Event 业务服务"""

    def __init__(self, db_factory: DbFactory):
        self._db_factory = db_factory
        self.event_repo = EventRepository(db_factory)
        self.session_repo = SessionRepository(db_factory)
        self.audit = AuditLogger(db_factory)
        self.permission = PermissionChecker(db_factory)

    def create_event(
        self,
        user_id: str,
        session_id: str,
        event_type: str,
        content: str,
        agent_id: str | None = None,
        agent_version: str | None = None,
        parent_event_id: str | None = None,
        causal_chain_id: str | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        """创建 Event

        Args:
            user_id: 用户ID
            session_id: Session ID
            event_type: 事件类型 (user_query, llm_response, tool_call, etc.)
            content: 事件内容
            agent_id: Agent ID
            agent_version: Agent版本
            parent_event_id: 父事件ID
            causal_chain_id: 因果链ID
            metadata: 元数据

        Returns:
            Event信息

        Raises:
            ValueError: Session不存在或无权限
        """
        # 1. 验证Session存在且有权限
        session = self.session_repo.get_by_id(session_id)
        if not session or session.user_id != user_id:
            # 统一返回"不存在"，避免泄露资源存在性
            raise ResourceNotFoundError(f"Session {session_id} 不存在")

        # 2. 设置默认值
        if metadata is None:
            metadata = {}

        # 3. 如果没有提供causal_chain_id，生成新的
        if causal_chain_id is None:
            from uuid_utils import uuid7

            causal_chain_id = str(uuid7())

        try:
            from uuid_utils import uuid7

            event_data = {
                "event_id": str(uuid7()),  # 生成event_id
                "user_id": user_id,
                "session_id": session_id,
                "event_type": event_type,
                "content": content,
                "agent_id": agent_id,
                "agent_version": agent_version,
                "parent_event_id": parent_event_id,
                "causal_chain_id": causal_chain_id,
                "event_metadata": metadata,  # 使用正确的字段名
            }

            event = self.event_repo.create(event_data)

            # 4. 更新Session的事件计数
            try:
                self.session_repo.update(session_id, {"event_count": session.event_count + 1})
            except:
                # 静默失败，不影响主流程
                pass

            # 5. 审计日志
            self.audit.log(
                user_id=user_id,
                action="event_create",
                resource_type="event",
                resource_id=event.event_id,
                details={"event_type": event_type, "session_id": session_id, "agent_id": agent_id},
                status="success",
            )

            return {
                "event_id": event.event_id,
                "user_id": event.user_id,
                "session_id": event.session_id,
                "event_type": event.event_type,
                "content": event.content,
                "agent_id": event.agent_id,
                "agent_version": event.agent_version,
                "parent_event_id": event.parent_event_id,
                "causal_chain_id": event.causal_chain_id,
                "metadata": event.event_metadata or {},
                "created_at": event.created_at.isoformat(),
            }

        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="event_create",
                resource_type="event",
                resource_id="unknown",
                details={"error": str(e)},
                status="failed",
            )
            raise

    def get_event(self, event_id: str, user_id: str) -> dict[str, Any]:
        """获取 Event 信息

        Args:
            event_id: Event ID
            user_id: 用户ID

        Returns:
            Event信息

        Raises:
            ValueError: Event不存在或无权限
        """
        event = self.event_repo.get_by_id(event_id)

        if not event:
            raise ResourceNotFoundError(f"Event {event_id} 不存在")

        # 权限检查 - 只能访问自己的Event
        if event.user_id != user_id:
            raise PermissionDeniedError(f"无权限访问 Event {event_id}")

        return {
            "event_id": event.event_id,
            "user_id": event.user_id,
            "session_id": event.session_id,
            "event_type": event.event_type,
            "content": event.content,
            "agent_id": event.agent_id,
            "agent_version": event.agent_version,
            "parent_event_id": event.parent_event_id,
            "causal_chain_id": event.causal_chain_id,
            "metadata": event.event_metadata or {},
            "created_at": event.created_at.isoformat(),
        }

    def list_events(
        self,
        user_id: str,
        session_id: str | None = None,
        event_type: str | None = None,
        agent_id: str | None = None,
        causal_chain_id: str | None = None,
        limit: int = 50,
        offset: int = 0,
    ) -> dict[str, Any]:
        """列出用户的 Events

        Args:
            user_id: 用户ID
            session_id: 过滤Session ID
            event_type: 过滤事件类型
            agent_id: 过滤Agent ID
            causal_chain_id: 过滤因果链ID
            limit: 限制数量
            offset: 偏移量

        Returns:
            Events列表和总数
        """
        events, total = self.event_repo.get_by_user(
            user_id=user_id,
            session_id=session_id,
            event_type=event_type,
            agent_id=agent_id,
            causal_chain_id=causal_chain_id,
            limit=limit,
            offset=offset,
        )

        return {
            "events": [
                {
                    "event_id": event.event_id,
                    "user_id": event.user_id,
                    "session_id": event.session_id,
                    "event_type": event.event_type,
                    "content": event.content,
                    "agent_id": event.agent_id,
                    "agent_version": event.agent_version,
                    "parent_event_id": event.parent_event_id,
                    "causal_chain_id": event.causal_chain_id,
                    "metadata": event.event_metadata or {},
                    "created_at": event.created_at.isoformat(),
                }
                for event in events
            ],
            "total": total,
            "limit": limit,
            "offset": offset,
        }

    def get_causal_chain(self, causal_chain_id: str, user_id: str) -> list[dict[str, Any]]:
        """获取因果链中的所有事件

        Args:
            causal_chain_id: 因果链ID
            user_id: 用户ID

        Returns:
            因果链中的所有事件，按时间排序
        """
        events = self.event_repo.get_by_causal_chain(causal_chain_id, user_id)

        return [
            {
                "event_id": event.event_id,
                "user_id": event.user_id,
                "session_id": event.session_id,
                "event_type": event.event_type,
                "content": event.content,
                "agent_id": event.agent_id,
                "agent_version": event.agent_version,
                "parent_event_id": event.parent_event_id,
                "causal_chain_id": event.causal_chain_id,
                "metadata": event.event_metadata or {},
                "created_at": event.created_at.isoformat(),
            }
            for event in events
        ]

    def get_session_events(
        self, session_id: str, user_id: str, limit: int = 100, offset: int = 0
    ) -> dict[str, Any]:
        """获取Session中的所有事件

        Args:
            session_id: Session ID
            user_id: 用户ID
            limit: 限制数量
            offset: 偏移量

        Returns:
            Session中的事件列表

        Raises:
            ValueError: Session不存在或无权限
        """
        # 验证Session权限
        session = self.session_repo.get_by_id(session_id)
        if not session:
            raise ValueError(f"Session {session_id} 不存在")

        if session.user_id != user_id:
            raise ValueError(f"无权限访问 Session {session_id}")

        events, total = self.event_repo.get_by_session(
            session_id=session_id, limit=limit, offset=offset
        )

        return {
            "events": [
                {
                    "event_id": event.event_id,
                    "user_id": event.user_id,
                    "session_id": event.session_id,
                    "event_type": event.event_type,
                    "content": event.content,
                    "agent_id": event.agent_id,
                    "agent_version": event.agent_version,
                    "parent_event_id": event.parent_event_id,
                    "causal_chain_id": event.causal_chain_id,
                    "metadata": event.event_metadata or {},
                    "created_at": event.created_at.isoformat(),
                }
                for event in events
            ],
            "total": total,
            "limit": limit,
            "offset": offset,
        }

    def delete_event(self, event_id: str, user_id: str) -> None:
        """删除 Event

        Args:
            event_id: Event ID
            user_id: 用户ID

        Raises:
            ValueError: Event不存在或无权限
        """
        event = self.event_repo.get_by_id(event_id)

        if not event:
            raise ValueError(f"Event {event_id} 不存在")

        # 权限检查
        if event.user_id != user_id:
            raise ValueError(f"无权限删除 Event {event_id}")

        try:
            self.event_repo.delete(event_id)

            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="event_delete",
                resource_type="event",
                resource_id=event_id,
                details={"event_type": event.event_type},
                status="success",
            )

        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="event_delete",
                resource_type="event",
                resource_id=event_id,
                details={"error": str(e)},
                status="failed",
            )
            raise
