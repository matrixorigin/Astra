"""Multi-tenancy support for enterprise deployment - ORM Version."""

from dataclasses import dataclass
from enum import Enum
from datetime import date

from core.exceptions import AuthenticationError
from core.logging_config import get_logger
from sqlalchemy.orm import Session
from sqlalchemy import func
from api.database import get_db_session
from api.models import Session as SessionModel

logger = get_logger(__name__)


class TenantStatus(str, Enum):
    """Tenant status."""
    ACTIVE = "active"
    SUSPENDED = "suspended"
    TRIAL = "trial"


@dataclass
class Tenant:
    """Tenant information."""
    tenant_id: str
    name: str
    status: TenantStatus
    max_users: int
    max_sessions_per_day: int
    max_llm_cost_per_day: float
    created_at: str
    metadata: dict


class TenantModel:
    """ORM model placeholder for tenants table."""
    # TODO: Add to api/models.py
    pass


class TenantManager:
    """Manage multi-tenant isolation using ORM."""

    def __init__(self, db: Session):
        self.db = db

    def get_tenant(self, tenant_id: str) -> Tenant | None:
        """Get tenant by ID using ORM."""
        # TODO: Replace with actual Tenant ORM model when added to api/models.py
        from sqlalchemy import text
        result = self.db.execute(
            text("SELECT * FROM tenants WHERE tenant_id = :tenant_id"),
            {"tenant_id": tenant_id}
        ).first()

        if not result:
            return None

        return Tenant(
            tenant_id=result.tenant_id,
            name=result.name,
            status=TenantStatus(result.status),
            max_users=result.max_users,
            max_sessions_per_day=result.max_sessions_per_day,
            max_llm_cost_per_day=result.max_llm_cost_per_day,
            created_at=str(result.created_at),
            metadata=result.metadata or {},
        )

    def check_quota(self, tenant_id: str) -> dict:
        """Check tenant quota usage using ORM."""
        tenant = self.get_tenant(tenant_id)
        if not tenant:
            raise AuthenticationError(f"Tenant not found: {tenant_id}")

        today = date.today()

        # Check daily sessions using ORM
        sessions_today = self.db.query(SessionModel).filter(
            SessionModel.tenant_id == tenant_id,
            func.date(SessionModel.created_at) == today
        ).count()

        # Check daily LLM cost using ORM
        # TODO: Add LLMCallLog ORM model
        from sqlalchemy import text
        cost_result = self.db.execute(
            text("""
                SELECT COALESCE(SUM(cost), 0) as total
                FROM llm_call_logs
                WHERE tenant_id = :tenant_id
                AND DATE(created_at) = CURDATE()
            """),
            {"tenant_id": tenant_id}
        ).first()
        cost_today = cost_result.total if cost_result else 0

        # Check user count using ORM
        user_count = self.db.query(func.count(func.distinct(SessionModel.user_id))).filter(
            SessionModel.tenant_id == tenant_id
        ).scalar()

        return {
            "sessions_today": sessions_today,
            "sessions_limit": tenant.max_sessions_per_day,
            "sessions_remaining": max(0, tenant.max_sessions_per_day - sessions_today),
            "cost_today": float(cost_today),
            "cost_limit": tenant.max_llm_cost_per_day,
            "cost_remaining": max(0, tenant.max_llm_cost_per_day - float(cost_today)),
            "users": user_count or 0,
            "users_limit": tenant.max_users,
            "status": tenant.status.value,
        }

    def enforce_quota(self, tenant_id: str):
        """Enforce tenant quotas."""
        quota = self.check_quota(tenant_id)

        if quota["sessions_remaining"] <= 0:
            raise AuthenticationError(
                f"Tenant {tenant_id} exceeded daily session limit "
                f"({quota['sessions_limit']})"
            )

        if quota["cost_remaining"] <= 0:
            raise AuthenticationError(
                f"Tenant {tenant_id} exceeded daily cost limit "
                f"(${quota['cost_limit']:.2f})"
            )
