"""Seed default roles for RBAC."""

from uuid import uuid4
from sqlalchemy.orm import Session
from sqlalchemy import text


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
        existing = db.execute(
            text("SELECT 1 FROM auth_roles WHERE role_name = :name"),
            {"name": role["role_name"]},
        ).fetchone()
        if existing:
            continue
        
        db.execute(
            text(
                "INSERT INTO auth_roles (role_id, role_name, description) "
                "VALUES (:id, :name, :desc)"
            ),
            {
                "id": role["role_id"],
                "name": role["role_name"],
                "desc": role["description"],
            },
        )
        count += 1
    
    db.commit()
    return count
