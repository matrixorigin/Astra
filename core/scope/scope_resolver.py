"""Scope-based configuration resolver with Open Scope Protocol - ORM Version.

Supports extensible scope chains for different business scenarios:
- Dev Agent: repo > project > user > global
- Sales Agent: region > sales_group > user > global
- Deploy Agent: environment > project > global
"""

from sqlalchemy.orm import Session
from api.database import get_db_session
from api.models import Config, Token


class ScopeResolver:
    """Resolve configuration with extensible scope chain using ORM."""

    def __init__(self, db: Session, scope_chain: list[tuple[str, str | None]]):
        """Initialize resolver with priority chain.

        Args:
            db: Session connection
            scope_chain: Priority chain from specific to general, e.g.,
                [('repo', 'matrixone'), ('project', 'backend'),
                 ('user', 'alice'), ('global', None)]
        """
        self.db = db
        self.scope_chain = scope_chain

    def resolve_config(self, key_name: str) -> str | None:
        """Resolve config value by scope chain.

        Returns first matching config from most specific to most general scope.
        """
        for scope_type, scope_id in self.scope_chain:
            query = self.db.query(Config).filter(
                Config.key_name == key_name,
                Config.scope_type == scope_type
            )
            
            if scope_id is not None:
                query = query.filter(Config.scope_user_id == scope_id)
            else:
                query = query.filter(Config.scope_user_id.is_(None))
            
            config = query.first()
            if config:
                return config.value
        
        return None

    def resolve_token(self, token_type: str, provider: str) -> dict | None:
        """Resolve API token by scope chain.

        Returns first matching token from most specific to most general scope.
        """
        for scope_type, scope_id in self.scope_chain:
            query = self.db.query(Token).filter(
                Token.type == token_type,
                Token.provider == provider,
                Token.is_active == True
            )
            
            # Match scope based on scope_type
            if scope_type == "user" and scope_id:
                query = query.filter(Token.scope_user_id == scope_id)
            elif scope_type == "repo" and scope_id:
                query = query.filter(Token.scope_repo == scope_id)
            elif scope_type == "global":
                query = query.filter(
                    Token.scope_user_id.is_(None),
                    Token.scope_repo.is_(None)
                )
            
            token = query.first()
            if token:
                return {
                    "token_id": token.token_id,
                    "provider": token.provider,
                    "encrypted_value": token.encrypted_value,
                    "secret_ref": token.secret_ref,
                }
        
        return None

    def list_tokens(self, token_type: str) -> list[dict]:
        """List all accessible tokens across scope chain.

        Returns tokens from all scopes in the chain.
        """
        tokens = {}
        
        # Iterate in reverse order (general to specific)
        for scope_type, scope_id in reversed(self.scope_chain):
            query = self.db.query(Token).filter(
                Token.token_type == token_type,
                Token.scope_type == scope_type,
                Token.is_active == True
            )
            
            if scope_id is not None:
                query = query.filter(Token.scope_user_id == scope_id)
            else:
                query = query.filter(Token.scope_user_id.is_(None))
            
            for token in query.all():
                # More specific scope overrides general
                tokens[token.provider] = {
                    "token_id": token.token_id,
                    "provider": token.provider,
                    "encrypted_value": token.encrypted_value,
                    "secret_ref": token.secret_ref,
                }
        
        return list(tokens.values())


class ScopeChainBuilder:
    """Build scope chains for different contexts."""

    @staticmethod
    def dev_agent(user_id: str | None = None, repo: str | None = None, project: str | None = None) -> list[tuple[str, str | None]]:
        """Build scope chain for dev agent context."""
        chain = []
        if repo:
            chain.append(("repo", repo))
        if project:
            chain.append(("project", project))
        if user_id:
            chain.append(("user", user_id))
        chain.append(("global", None))
        return chain

    @staticmethod
    def sales_agent(user_id: str | None = None, region: str | None = None, sales_group: str | None = None) -> list[tuple[str, str | None]]:
        """Build scope chain for sales agent context."""
        chain = []
        if region:
            chain.append(("region", region))
        if sales_group:
            chain.append(("sales_group", sales_group))
        if user_id:
            chain.append(("user", user_id))
        chain.append(("global", None))
        return chain

    @staticmethod
    def deploy_agent(user_id: str | None = None, environment: str | None = None, project: str | None = None) -> list[tuple[str, str | None]]:
        """Build scope chain for deploy agent context."""
        chain = []
        if environment:
            chain.append(("environment", environment))
        if project:
            chain.append(("project", project))
        chain.append(("global", None))
        return chain

    @staticmethod
    def custom(user_id: str | None = None, custom_scopes: list[tuple[str, str]] | None = None) -> list[tuple[str, str | None]]:
        """Build custom scope chain."""
        chain = []
        if custom_scopes:
            chain.extend(custom_scopes)
        if user_id:
            chain.append(("user", user_id))
        chain.append(("global", None))
        return chain

    @staticmethod
    def for_user(user_id: str) -> list[tuple[str, str | None]]:
        """Build scope chain for user context."""
        chain = [("user", user_id)]
        chain.append(("global", None))
        return chain

    @staticmethod
    def for_repo(repo_id: str, user_id: str) -> list[tuple[str, str | None]]:
        """Build scope chain for repo context."""
        chain = [("repo", repo_id), ("user", user_id)]
        chain.append(("global", None))
        return chain
