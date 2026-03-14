"""Sandbox Service - 业务逻辑层

提供 Sandbox 管理的业务逻辑，包括：
- 权限检查
- 审计日志
- 数据验证
"""

from datetime import datetime, timezone
from typing import Any

from sqlalchemy.orm import Session

from core.auth.audit_logger import AuditLogger
from core.auth.permission_checker import PermissionChecker
from core.db_consumer import DbFactory
from core.sandbox import Sandbox


class SandboxService:
    """Sandbox 业务服务"""

    def __init__(self, db_factory: DbFactory):
        """Initialize service.

        Args:
            db_factory: Callable that returns the current request-scoped Session.
        """
        self._db_factory = db_factory
        from config.settings import get_settings

        self.sandbox = Sandbox(db_factory=db_factory, source_db=get_settings().matrixone_database)
        self.audit = AuditLogger(db_factory)
        self.permission = PermissionChecker(db_factory)

    def create_sandbox(
        self, name: str, user_id: str, description: str = "", created_by: str = ""
    ) -> dict[str, Any]:
        """创建 sandbox

        Args:
            name: Sandbox 名称
            user_id: 用户ID
            description: 描述
            created_by: 创建者

        Returns:
            Sandbox 信息

        Raises:
            PermissionError: 权限不足
            ValueError: 参数错误
        """
        # 1. 权限检查 (开发模式: 允许所有操作)
        # 生产模式需要检查: self.permission.has_role(user_id, "mo_agent_user")
        # 当前开发模式: 跳过RBAC检查
        pass

        # 2. 参数验证
        if not name or not name.strip():
            raise ValueError("Sandbox name 不能为空")

        # 3. 创建 sandbox
        try:
            self.sandbox.create(
                name=name, description=description, created_by=created_by or user_id
            )

            # 4. 审计日志
            self.audit.log(
                user_id=user_id,
                action="sandbox_create",
                resource_type="sandbox",
                resource_id=name,
                details={"description": description},
                status="success",
            )

            # 5. 返回结果
            return {
                "sandbox_name": name,
                "description": description,
                "created_by": created_by or user_id,
                "created_at": datetime.now(timezone.utc).isoformat(),
            }

        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="sandbox_create",
                resource_type="sandbox",
                resource_id=name,
                details={"error": str(e)},
                status="failed",
            )
            raise

    def list_sandboxes(self, user_id: str, pattern: str = "%") -> list[dict[str, Any]]:
        """列出 sandboxes

        Args:
            user_id: 用户ID
            pattern: 过滤模式

        Returns:
            Sandbox 列表
        """
        # 开发模式: 返回所有sandboxes
        # 生产模式: 需要权限检查和过滤
        sandboxes = self.sandbox.list_sandboxes(prefix="", pattern=pattern)

        # Convert datetime to string for API response
        for sandbox in sandboxes:
            if sandbox.get("created_at"):
                sandbox["created_at"] = sandbox["created_at"].isoformat()
            if sandbox.get("updated_at"):
                sandbox["updated_at"] = sandbox["updated_at"].isoformat()

        return sandboxes

    def delete_sandbox(self, name: str, user_id: str) -> None:
        """删除 sandbox

        Args:
            name: Sandbox 名称
            user_id: 用户ID

        Raises:
            ValueError: Sandbox 不存在
        """
        # 开发模式: 允许删除任何sandbox
        # 检查 sandbox 是否存在
        sandboxes = self.sandbox.list_sandboxes(prefix="", pattern=name)
        if not any(s["sandbox_name"] == name for s in sandboxes):
            raise ValueError(f"Sandbox {name} 不存在")

        # 删除 sandbox
        try:
            self.sandbox.delete(name)

            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="sandbox_delete",
                resource_type="sandbox",
                resource_id=name,
                details={},
                status="success",
            )

        except Exception as e:
            self.audit.log(
                user_id=user_id,
                action="sandbox_delete",
                resource_type="sandbox",
                resource_id=name,
                details={"error": str(e)},
                status="failed",
            )
            raise

    def get_sandbox_info(self, name: str, user_id: str) -> dict[str, Any]:
        """获取 sandbox 信息

        Args:
            name: Sandbox 名称
            user_id: 用户ID

        Returns:
            Sandbox 信息

        Raises:
            ValueError: Sandbox 不存在
        """
        # 开发模式: 允许查看任何sandbox
        sandboxes = self.sandbox.list_sandboxes(prefix="", pattern=name)
        sandbox_info = next((s for s in sandboxes if s["sandbox_name"] == name), None)

        if not sandbox_info:
            raise ValueError(f"Sandbox {name} 不存在")

        # Convert datetime to string for API response
        if sandbox_info.get("created_at"):
            sandbox_info["created_at"] = sandbox_info["created_at"].isoformat()
        if sandbox_info.get("updated_at"):
            sandbox_info["updated_at"] = sandbox_info["updated_at"].isoformat()

        return sandbox_info
