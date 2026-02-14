"""Permission checker using App-Layer RBAC."""

from sqlalchemy.orm import Session
from api.models import User, Role, UserRole


class PermissionChecker:
    """Check user permissions using App-Layer RBAC."""

    def __init__(self, db: Session):
        self.db = db

    def has_role(self, user_id: str, role_name: str) -> bool:
        """Check if user has a specific role.
        
        Args:
            user_id: User UUID or Username
            role_name: Role name (e.g., 'mo_agent_admin')
        """
        query = (
            self.db.query(UserRole)
            .join(Role, UserRole.role_id == Role.role_id)
            .join(User, UserRole.user_id == User.user_id)
            .filter(Role.role_name == role_name)
        )
        
        # Support both UUID (user_id) and Username
        query = query.filter((User.user_id == user_id) | (User.username == user_id))
        
        return query.count() > 0

    def is_admin(self, user_id: str) -> bool:
        """Check if user is mo_agent_admin."""
        return self.has_role(user_id, "mo_agent_admin")

    def is_user(self, user_id: str) -> bool:
        """Check if user is mo_agent_user."""
        return self.has_role(user_id, "mo_agent_user")

    def can_manage_models(self, user_id: str, scope: str, scope_id: str | None = None) -> bool:
        """Check if user can manage models at given scope.

        Enforces strict RBAC:
        - Global scope: Admin only
        - Account scope: Admin only (or account owner - simplified to admin for now)
        - User scope: Self or Admin
        """
        if self.is_admin(user_id):
            return True

        if scope == "global":
            return False  # Only admin can manage global models
            
        if scope == "account":
            return False  # Only admin can manage account models for now
            
        if scope == "user" and scope_id == user_id:
            return self.is_user(user_id)
            
        return False

    def can_manage_skills(self, user_id: str, scope: str, scope_id: str | None = None) -> bool:
        """Check if user can manage skills at given scope.
        
        Enforces strict RBAC:
        - Global/Account scope: Admin only
        - User scope: Self or Admin
        """
        if self.is_admin(user_id):
            return True
            
        if scope in ["global", "account"]:
            return False
            
        if scope == "user" and scope_id == user_id:
            return self.is_user(user_id)
            
        return False

    def can_view_audit_logs(self, user_id: str, target_user: str | None = None) -> bool:
        """Check if user can view audit logs."""
        # Admin can view all logs
        if self.is_admin(user_id):
            return True

        # Users can view their own logs
        if target_user == user_id:
            return self.is_user(user_id)

        return False
