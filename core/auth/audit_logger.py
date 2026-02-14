"""Audit logger for tracking admin operations."""

import json
from datetime import datetime
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session


class AuditLogger:
    """Log admin operations to audit_logs table."""

    def __init__(self, db: Session):
        self.db = db

    def log(
        self,
        user_id: str,
        action: str,
        resource_type: str,
        resource_id: str,
        details: dict[str, Any] | None = None,
        status: str = "success",
    ):
        """Log an admin operation."""
        # Generate log_id
        import uuid

        log_id = f"log_{uuid.uuid4().hex[:16]}"

        from sqlalchemy import insert
        from api.models import AuditLog
        
        self.db.execute(
            insert(AuditLog).values(
                log_id=log_id,
                user_id=user_id,
                action=action,
                resource_type=resource_type,
                resource_id=resource_id,
                details=details,
                created_at=datetime.now(),
            )
        )
        self.db.commit()

    def log_model_add(self, user_id: str, model_name: str, scope: str, scope_id: str | None = None):
        """Log model addition."""
        self.log(
            user_id=user_id,
            action="add_model",
            resource_type="model",
            resource_id=model_name,
            details={"scope": scope, "scope_id": scope_id},
        )

    def log_model_remove(
        self, user_id: str, model_name: str, scope: str, scope_id: str | None = None
    ):
        """Log model removal."""
        self.log(
            user_id=user_id,
            action="remove_model",
            resource_type="model",
            resource_id=model_name,
            details={"scope": scope, "scope_id": scope_id},
        )

    def log_model_update(self, user_id: str, model_name: str, changes: dict[str, Any]):
        """Log model update."""
        self.log(
            user_id=user_id,
            action="update_model",
            resource_type="model",
            resource_id=model_name,
            details={"changes": changes},
        )

    def log_skill_register(self, user_id: str, skill_name: str, scope: str):
        """Log skill registration."""
        self.log(
            user_id=user_id,
            action="register_skill",
            resource_type="skill",
            resource_id=skill_name,
            details={"scope": scope},
        )

    def log_token_create(self, user_id: str, token_type: str, provider: str, scope: str):
        """Log token creation."""
        self.log(
            user_id=user_id,
            action="create_token",
            resource_type="token",
            resource_id=f"{token_type}_{provider}",
            details={"scope": scope},
        )

    def get_logs(
        self,
        user_id: str | None = None,
        action: str | None = None,
        resource_type: str | None = None,
        since: datetime | None = None,
        limit: int = 100,
    ):
        """Query audit logs."""
        from sqlalchemy import text
        
        conditions = []
        params = {}

        if user_id:
            conditions.append("user_id = :user_id")
            params["user_id"] = user_id

        if action:
            conditions.append("action = :action")
            params["action"] = action

        if resource_type:
            conditions.append("resource_type = :resource_type")
            params["resource_type"] = resource_type

        if since:
            conditions.append("created_at >= :since")
            params["since"] = since.isoformat() if isinstance(since, datetime) else since

        where_clause = " AND ".join(conditions) if conditions else "1=1"

        query = text(f"""
        SELECT * FROM audit_logs
        WHERE {where_clause}
        ORDER BY created_at DESC
        LIMIT {limit}
        """)

        result = self.db.execute(query, params)
        return [dict(row._mapping) for row in result] if result else []
