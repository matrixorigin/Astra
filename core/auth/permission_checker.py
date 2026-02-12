"""Permission checker using MatrixOne RBAC."""

from sdk import Database


class PermissionChecker:
    """Check user permissions using MatrixOne RBAC."""

    def __init__(self, db: Database):
        self.db = db

    def has_role(self, user_id: str, role_name: str) -> bool:
        """Check if user has a specific role."""
        query = """
        SELECT COUNT(*) as cnt
        FROM mo_catalog.mo_user_grant ug
        JOIN mo_catalog.mo_user u ON ug.user_id = u.user_id
        JOIN mo_catalog.mo_role r ON ug.role_id = r.role_id
        WHERE u.user_name = %s AND r.role_name = %s
        """
        row = self.db.fetchone(query, (user_id, role_name))
        return bool(row and row.get("cnt", 0) > 0)

    def is_admin(self, user_id: str) -> bool:
        """Check if user is mo_agent_admin."""
        return self.has_role(user_id, "mo_agent_admin")

    def is_user(self, user_id: str) -> bool:
        """Check if user is mo_agent_user."""
        return self.has_role(user_id, "mo_agent_user")

    def can_manage_models(self, user_id: str, scope: str, scope_id: str | None = None) -> bool:
        """Check if user can manage models at given scope.
        
        For development: simplified to allow all operations.
        For production: uncomment RBAC checks below.
        """
        # Development mode: allow all
        return True
        
        # Production mode (uncomment when RBAC is set up):
        # if scope == "global":
        #     return self.is_admin(user_id)
        # if scope == "account":
        #     return self.is_admin(user_id)  # or check account membership
        # if scope == "user" and scope_id == user_id:
        #     return True
        # return False

    def can_manage_skills(self, user_id: str, scope: str, scope_id: str | None = None) -> bool:
        """Check if user can manage skills at given scope.
        
        For development: simplified to allow all operations.
        """
        # Development mode: allow all
        return True
        
        # Production mode (uncomment when RBAC is set up):
        # if scope in ["global", "account"]:
        #     return self.is_admin(user_id)
        # if scope == "user" and scope_id == user_id:
        #     return self.is_user(user_id)
        # return False

    def can_view_audit_logs(self, user_id: str, target_user: str | None = None) -> bool:
        """Check if user can view audit logs."""
        # Admin can view all logs
        if self.is_admin(user_id):
            return True

        # Users can view their own logs
        if target_user == user_id:
            return self.is_user(user_id)

        return False
