"""Audit logger for tracking admin operations."""

from datetime import datetime
from typing import Optional, Dict, Any
import json
from sdk import Database


class AuditLogger:
    """Log admin operations to audit_logs table."""
    
    def __init__(self, db: Database):
        self.db = db
    
    def log(
        self,
        user_id: str,
        action: str,
        resource_type: str,
        resource_id: str,
        details: Optional[Dict[str, Any]] = None,
        status: str = 'success'
    ):
        """Log an admin operation."""
        self.db.execute(
            """
            INSERT INTO agent_config.audit_logs 
            (user_id, action, resource_type, resource_id, details, status, created_at)
            VALUES (%s, %s, %s, %s, %s, %s, %s)
            """,
            (
                user_id,
                action,
                resource_type,
                resource_id,
                json.dumps(details) if details else None,
                status,
                datetime.now()
            )
        )
    
    def log_model_add(self, user_id: str, model_name: str, scope: str, scope_id: Optional[str] = None):
        """Log model addition."""
        self.log(
            user_id=user_id,
            action='add_model',
            resource_type='model',
            resource_id=model_name,
            details={'scope': scope, 'scope_id': scope_id}
        )
    
    def log_model_remove(self, user_id: str, model_name: str, scope: str, scope_id: Optional[str] = None):
        """Log model removal."""
        self.log(
            user_id=user_id,
            action='remove_model',
            resource_type='model',
            resource_id=model_name,
            details={'scope': scope, 'scope_id': scope_id}
        )
    
    def log_model_update(self, user_id: str, model_name: str, changes: Dict[str, Any]):
        """Log model update."""
        self.log(
            user_id=user_id,
            action='update_model',
            resource_type='model',
            resource_id=model_name,
            details={'changes': changes}
        )
    
    def log_skill_register(self, user_id: str, skill_name: str, scope: str):
        """Log skill registration."""
        self.log(
            user_id=user_id,
            action='register_skill',
            resource_type='skill',
            resource_id=skill_name,
            details={'scope': scope}
        )
    
    def log_token_create(self, user_id: str, token_type: str, provider: str, scope: str):
        """Log token creation."""
        self.log(
            user_id=user_id,
            action='create_token',
            resource_type='token',
            resource_id=f"{token_type}_{provider}",
            details={'scope': scope}
        )
    
    def get_logs(
        self,
        user_id: Optional[str] = None,
        action: Optional[str] = None,
        resource_type: Optional[str] = None,
        since: Optional[datetime] = None,
        limit: int = 100
    ):
        """Query audit logs."""
        conditions = []
        params = []
        
        if user_id:
            conditions.append("user_id = %s")
            params.append(user_id)
        
        if action:
            conditions.append("action = %s")
            params.append(action)
        
        if resource_type:
            conditions.append("resource_type = %s")
            params.append(resource_type)
        
        if since:
            conditions.append("created_at >= %s")
            params.append(since)
        
        where_clause = " AND ".join(conditions) if conditions else "1=1"
        
        query = f"""
        SELECT * FROM agent_config.audit_logs
        WHERE {where_clause}
        ORDER BY created_at DESC
        LIMIT {limit}
        """
        
        return self.db.fetchall(query, tuple(params))
