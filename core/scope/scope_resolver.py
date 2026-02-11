"""Scope-based configuration resolver with Open Scope Protocol.

Supports extensible scope chains for different business scenarios:
- Dev Agent: repo > project > user > account > global
- Sales Agent: region > sales_group > user > account > global
- Deploy Agent: environment > project > account > global
"""

from typing import Any, Optional
from sdk import Database


class ScopeResolver:
    """Resolve configuration with extensible scope chain."""
    
    def __init__(self, db: Database, scope_chain: list[tuple[str, Optional[str]]]):
        """Initialize resolver with priority chain.
        
        Args:
            db: Database connection
            scope_chain: Priority chain from specific to general, e.g.,
                [('repo', 'matrixone'), ('project', 'backend'), 
                 ('user', 'alice'), ('account', 'acme'), ('global', None)]
        """
        self.db = db
        self.scope_chain = scope_chain
    
    def resolve_model(self, model_name: str) -> Optional[dict]:
        """Resolve model config by scope chain.
        
        Returns first matching model config from most specific to most general scope.
        """
        for scope_type, scope_id in self.scope_chain:
            query = """
                SELECT * FROM agent_config.model_registry
                WHERE model_name = %s AND scope_type = %s
            """
            params = [model_name, scope_type]
            
            if scope_id is not None:
                query += " AND scope_id = %s"
                params.append(scope_id)
            else:
                query += " AND scope_id IS NULL"
            
            result = self.db.fetchone(query, tuple(params))
            if result:
                return dict(result)
        
        return None
    
    def resolve_token(self, token_type: str, provider: str) -> Optional[dict]:
        """Resolve API token by scope chain.
        
        Returns first matching token from most specific to most general scope.
        """
        for scope_type, scope_id in self.scope_chain:
            query = """
                SELECT * FROM agent_config.api_tokens
                WHERE token_type = %s AND provider = %s 
                AND scope_type = %s AND is_active = TRUE
            """
            params = [token_type, provider, scope_type]
            
            if scope_id is not None:
                query += " AND scope_id = %s"
                params.append(scope_id)
            else:
                query += " AND scope_id IS NULL"
            
            result = self.db.fetchone(query, tuple(params))
            if result:
                return dict(result)
        
        return None
    
    def list_models(self) -> list[dict]:
        """List all accessible models across scope chain.
        
        Returns models from all scopes in the chain, with more specific scopes
        overriding more general ones for the same model_name.
        """
        models = {}
        
        # Iterate in reverse order (general to specific)
        for scope_type, scope_id in reversed(self.scope_chain):
            query = """
                SELECT * FROM agent_config.model_registry
                WHERE scope_type = %s
            """
            params = [scope_type]
            
            if scope_id is not None:
                query += " AND scope_id = %s"
                params.append(scope_id)
            else:
                query += " AND scope_id IS NULL"
            
            results = self.db.fetchall(query, tuple(params))
            for row in results:
                # More specific scopes override general ones
                models[row['model_name']] = dict(row)
        
        return list(models.values())
    
    def list_skills(self) -> list[dict]:
        """List all accessible skills across scope chain."""
        skills = {}
        
        # Iterate in reverse order (general to specific)
        for scope_type, scope_id in reversed(self.scope_chain):
            query = """
                SELECT * FROM agent_config.skills_registry
                WHERE scope_type = %s AND is_active = TRUE
            """
            params = [scope_type]
            
            if scope_id is not None:
                query += " AND scope_id = %s"
                params.append(scope_id)
            else:
                query += " AND scope_id IS NULL"
            
            results = self.db.fetchall(query, tuple(params))
            for row in results:
                skills[row['skill_name']] = dict(row)
        
        return list(skills.values())


class ScopeChainBuilder:
    """Build scope chains for different scenarios."""
    
    @staticmethod
    def dev_agent(
        user_id: str,
        account_id: str,
        repo: Optional[str] = None,
        project: Optional[str] = None
    ) -> list[tuple[str, Optional[str]]]:
        """Build scope chain for Dev Agent scenario.
        
        Priority: repo > project > user > account > global
        """
        chain = []
        if repo:
            chain.append(('repo', repo))
        if project:
            chain.append(('project', project))
        chain.extend([
            ('user', user_id),
            ('account', account_id),
            ('global', None)
        ])
        return chain
    
    @staticmethod
    def sales_agent(
        user_id: str,
        account_id: str,
        region: Optional[str] = None,
        sales_group: Optional[str] = None
    ) -> list[tuple[str, Optional[str]]]:
        """Build scope chain for Sales Agent scenario.
        
        Priority: region > sales_group > user > account > global
        """
        chain = []
        if region:
            chain.append(('region', region))
        if sales_group:
            chain.append(('sales_group', sales_group))
        chain.extend([
            ('user', user_id),
            ('account', account_id),
            ('global', None)
        ])
        return chain
    
    @staticmethod
    def deploy_agent(
        user_id: str,
        account_id: str,
        environment: Optional[str] = None,
        project: Optional[str] = None
    ) -> list[tuple[str, Optional[str]]]:
        """Build scope chain for Deploy Agent scenario.
        
        Priority: environment > project > account > global
        """
        chain = []
        if environment:
            chain.append(('environment', environment))
        if project:
            chain.append(('project', project))
        chain.extend([
            ('account', account_id),
            ('global', None)
        ])
        return chain
    
    @staticmethod
    def custom(
        user_id: str,
        account_id: str,
        custom_scopes: list[tuple[str, str]]
    ) -> list[tuple[str, Optional[str]]]:
        """Build custom scope chain.
        
        Args:
            custom_scopes: List of (scope_type, scope_id) tuples in priority order
        """
        chain = list(custom_scopes)
        chain.extend([
            ('user', user_id),
            ('account', account_id),
            ('global', None)
        ])
        return chain
