"""Seed default roles for RBAC."""

from sqlalchemy.orm import Session

from api.models import Role

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
        if db.query(Role).filter(Role.role_name == role["role_name"]).first():
            continue
        db.add(Role(**role))
        count += 1
    db.commit()
    return count
