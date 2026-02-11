"""Multi-tenancy support for enterprise deployment."""

from dataclasses import dataclass
from enum import Enum

from core.exceptions import AuthenticationError
from core.logging_config import get_logger
from sdk import Database

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


class TenantManager:
    """Manage multi-tenant isolation."""

    def __init__(self, db: Database):
        self.db = db

    def create_tenant(
        self,
        name: str,
        max_users: int = 100,
        max_sessions_per_day: int = 1000,
        max_llm_cost_per_day: float = 100.0,
    ) -> Tenant:
        """Create new tenant."""
        from uuid_utils import uuid7

        tenant_id = str(uuid7())

        self.db.execute(
            """
            INSERT INTO tenants (
                tenant_id, name, status, max_users,
                max_sessions_per_day, max_llm_cost_per_day
            ) VALUES (%s, %s, %s, %s, %s, %s)
        """,
            (
                tenant_id,
                name,
                TenantStatus.ACTIVE.value,
                max_users,
                max_sessions_per_day,
                max_llm_cost_per_day,
            ),
        )

        logger.info(f"Created tenant: {tenant_id} ({name})")

        tenant = self.get_tenant(tenant_id)
        if not tenant:
            raise RuntimeError(f"Failed to create tenant: {tenant_id}")
        return tenant

    def get_tenant(self, tenant_id: str) -> Tenant | None:
        """Get tenant by ID."""
        row = self.db.fetchone("SELECT * FROM tenants WHERE tenant_id = %s", (tenant_id,))

        if not row:
            return None

        return Tenant(
            tenant_id=row.get("tenant_id", ""),
            name=row.get("name", ""),
            status=TenantStatus(row.get("status", "active")),
            max_users=row.get("max_users", 100),
            max_sessions_per_day=row.get("max_sessions_per_day", 1000),
            max_llm_cost_per_day=row.get("max_llm_cost_per_day", 100.0),
            created_at=str(row.get("created_at", "")),
            metadata=row.get("metadata", {}),
        )

    def check_quota(self, tenant_id: str) -> dict:
        """Check tenant quota usage."""
        tenant = self.get_tenant(tenant_id)
        if not tenant:
            raise AuthenticationError(f"Tenant not found: {tenant_id}")

        # Check daily sessions
        sessions_row = self.db.fetchone(
            """
            SELECT COUNT(*) as count
            FROM sessions
            WHERE tenant_id = %s
            AND DATE(created_at) = CURDATE()
        """,
            (tenant_id,),
        )
        sessions_today = sessions_row.get("count", 0) if sessions_row else 0

        # Check daily LLM cost
        cost_row = self.db.fetchone(
            """
            SELECT COALESCE(SUM(cost), 0) as total
            FROM llm_call_logs
            WHERE tenant_id = %s
            AND DATE(created_at) = CURDATE()
        """,
            (tenant_id,),
        )
        cost_today = cost_row.get("total", 0) if cost_row else 0

        # Check user count
        user_row = self.db.fetchone(
            """
            SELECT COUNT(DISTINCT user_id) as count
            FROM sessions
            WHERE tenant_id = %s
        """,
            (tenant_id,),
        )
        user_count = user_row.get("count", 0) if user_row else 0

        return {
            "sessions_today": sessions_today,
            "sessions_limit": tenant.max_sessions_per_day,
            "sessions_remaining": max(0, tenant.max_sessions_per_day - sessions_today),
            "cost_today": float(cost_today),
            "cost_limit": tenant.max_llm_cost_per_day,
            "cost_remaining": max(0, tenant.max_llm_cost_per_day - float(cost_today)),
            "users": user_count,
            "users_limit": tenant.max_users,
            "status": tenant.status.value,
        }

    def enforce_quota(self, tenant_id: str):
        """Enforce tenant quotas."""
        quota = self.check_quota(tenant_id)

        if quota["sessions_remaining"] <= 0:
            raise AuthenticationError("Daily session quota exceeded")

        if quota["cost_remaining"] <= 0:
            raise AuthenticationError("Daily cost quota exceeded")

        if quota["users"] >= quota["users_limit"]:
            raise AuthenticationError("User limit reached")


# Schema migration
TENANT_SCHEMA = """
-- Add tenants table
CREATE TABLE IF NOT EXISTS tenants (
    tenant_id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'active',
    max_users INT NOT NULL DEFAULT 100,
    max_sessions_per_day INT NOT NULL DEFAULT 1000,
    max_llm_cost_per_day DECIMAL(10, 2) NOT NULL DEFAULT 100.00,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    metadata JSON,

    INDEX idx_status (status),
    INDEX idx_created (created_at)
);

-- Add tenant_id to existing tables
ALTER TABLE sessions ADD COLUMN tenant_id VARCHAR(36);
ALTER TABLE conversation_events ADD COLUMN tenant_id VARCHAR(36);
ALTER TABLE llm_call_logs ADD COLUMN tenant_id VARCHAR(36);

-- Add indexes
CREATE INDEX idx_sessions_tenant ON sessions(tenant_id);
CREATE INDEX idx_events_tenant ON conversation_events(tenant_id);
CREATE INDEX idx_llm_logs_tenant ON llm_call_logs(tenant_id);
"""
