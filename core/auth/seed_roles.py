"""Seed default roles for RBAC."""

from sqlalchemy import text
from sqlalchemy.orm import Session

SEED_ROLES = [
    {
        "role_id": "role-admin",
        "role_name": "mo_agent_admin",
        "description": "Administrator with full system access",
    },
    {
        "role_id": "role-user",
        "role_name": "mo_agent_user",
        "description": "Regular user with limited access",
    },
]
def seed_roles(db: Session) -> int:
    """Insert default roles if they don't exist. Returns count of inserted roles."""
    count = 0
    for role in SEED_ROLES:
        result = db.execute(
            text(
                "INSERT IGNORE INTO auth_roles (role_id, role_name, description) "
                "VALUES (:role_id, :role_name, :description)"
            ),
            role,
        )
        count += int(result.rowcount > 0)
    db.commit()
    return count
