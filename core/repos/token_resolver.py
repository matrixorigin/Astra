"""Token resolution service.

Implements the token resolution priority from design doc:
1. Repo-specific token
2. User default token
3. Tenant default token
4. Global fallback (if allowed)
"""

import json
from datetime import datetime, timezone

from sqlalchemy import text
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from api.database import get_db_session
from core.repos.token_models import Token, TokenType


class TokenResolver:
    """Token resolution service."""

    def __init__(self, db: Session | None = None) -> None:
        self.db = db or next(get_db_session())

    def resolve_repo_token(
        self,
        user_id: str,
        tenant_id: str | None = None,
        repo_url: str | None = None,
        repo_id: str | None = None,
    ) -> Token | None:
        """Resolve repo token with priority fallback.

        Priority:
        1. Repo-specific token (from repos.token_id)
        2. User default token (scope_user_id, no scope_repo)
        3. Tenant default token (scope_tenant_id, no scope_repo)
        4. Global fallback (if config allows)
        """
        # 1. Repo-specific token
        if repo_id or repo_url:
            token = self._get_repo_specific_token(repo_id, repo_url, user_id)
            if token:
                return token

        # 2. User default token
        token = self._get_user_default_token(user_id)
        if token:
            return token

        # 3. Tenant default token
        if tenant_id:
            token = self._get_tenant_default_token(tenant_id)
            if token:
                return token

        # 4. Global fallback (check config)
        if self._allow_global_token():
            return self._get_global_token()

        return None

    def create_token(
        self,
        token_type: TokenType,
        provider: str,
        secret_ref: str | None = None,
        encrypted_value: str | None = None,
        scope_user_id: str | None = None,
        scope_tenant_id: str | None = None,
        scope_repo: str | None = None,
        expires_at: datetime | None = None,
        metadata: dict | None = None,
    ) -> Token:
        """Create a new token."""
        from api.models import Token as TokenModel
        token_id = str(uuid7())
        now = datetime.now(timezone.utc)

        token_model = TokenModel(
            token_id=token_id,
            type=token_type.value,
            provider=provider,
            scope_user_id=scope_user_id,
            scope_tenant_id=scope_tenant_id,
            scope_repo=scope_repo,
            secret_ref=secret_ref,
            encrypted_value=encrypted_value,
            is_active=True,
            expires_at=expires_at,
            created_at=now,
            token_metadata=metadata or {},
        )
        self.db.add(token_model)
        self.db.commit()
        self.db.refresh(token_model)

        return Token(
            token_id=token_id,
            token_type=token_type,
            provider=provider,
            scope_user_id=scope_user_id,
            scope_tenant_id=scope_tenant_id,
            scope_repo=scope_repo,
            secret_ref=secret_ref,
            encrypted_value=encrypted_value,
            is_active=True,
            expires_at=expires_at,
            created_at=now,
            metadata=metadata or {},
        )

    def get_token(self, token_id: str) -> Token | None:
        """Get token by ID."""
        from api.models import Token as TokenModel
        result = self.db.query(TokenModel).filter(TokenModel.token_id == token_id).first()
        if not result:
            return None
        return self._to_model(result)

    def deactivate_token(self, token_id: str) -> None:
        """Deactivate token (e.g., on 401 error)."""
        from api.models import Token as TokenModel
        self.db.query(TokenModel).filter(TokenModel.token_id == token_id).update({"is_active": False})
        self.db.commit()
        self.db.expire_all()  # Clear session cache

    def _get_repo_specific_token(
        self, repo_id: str | None, repo_url: str | None, user_id: str
    ) -> Token | None:
        """Get repo-specific token from repos table."""
        from api.models import Token as TokenModel, Repo as RepoModel
        
        query = self.db.query(TokenModel).join(
            RepoModel, RepoModel.token_id == TokenModel.token_id
        ).filter(TokenModel.is_active == 1)
        
        if repo_id:
            query = query.filter(RepoModel.repo_id == repo_id, RepoModel.user_id == user_id)
        elif repo_url:
            query = query.filter(RepoModel.repo_url == repo_url, RepoModel.user_id == user_id)
        else:
            return None
        
        result = query.first()
        return self._to_model(result) if result else None

    def _get_user_default_token(self, user_id: str) -> Token | None:
        """Get user default token (no scope_repo)."""
        from api.models import Token as TokenModel
        result = self.db.query(TokenModel).filter(
            TokenModel.type == 'repo',
            TokenModel.scope_user_id == user_id,
            TokenModel.scope_repo.is_(None),
            TokenModel.is_active == 1
        ).order_by(TokenModel.created_at.desc()).first()
        return self._to_model(result) if result else None

    def _get_tenant_default_token(self, tenant_id: str) -> Token | None:
        """Get tenant default token (no scope_repo)."""
        from api.models import Token as TokenModel
        result = self.db.query(TokenModel).filter(
            TokenModel.type == 'repo',
            TokenModel.scope_tenant_id == tenant_id,
            TokenModel.scope_repo.is_(None),
            TokenModel.is_active == 1
        ).order_by(TokenModel.created_at.desc()).first()
        return self._to_model(result) if result else None

    def _get_global_token(self) -> Token | None:
        """Get global fallback token."""
        query = """
            SELECT * FROM tokens
            WHERE type = 'repo'
              AND scope_user_id IS NULL
              AND scope_tenant_id IS NULL
              AND scope_repo IS NULL
              AND is_active = TRUE
            ORDER BY created_at DESC
            LIMIT 1
        """
        result = self.db.query(Token).filter(
            Token.token_type == token_type,
            Token.provider == provider,
            Token.scope_type == scope_type,
            Token.is_active == True
        ).order_by(Token.created_at.desc()).first()
        
        return result if result else None

    def _allow_global_token(self) -> bool:
        """Check if global token fallback is allowed using ORM."""
        from sqlalchemy import text
        
        result = self.db.execute(
            text("SELECT value FROM configs WHERE key_name = 'allow_global_repo_token'")
        ).first()
        
        if not result:
            return False
        return result[0].lower() in ("true", "1", "yes")

    def _to_model(self, row) -> Token:
        """Convert ORM object to Token model."""
        metadata = getattr(row, 'token_metadata', None)
        if isinstance(metadata, str):
            metadata = json.loads(metadata)
        
        return Token(
            token_id=row.token_id,
            token_type=TokenType(row.type),
            provider=row.provider,
            scope_user_id=row.scope_user_id,
            scope_tenant_id=row.scope_tenant_id,
            scope_repo=row.scope_repo,
            secret_ref=row.secret_ref,
            encrypted_value=row.encrypted_value,
            is_active=bool(row.is_active),
            expires_at=row.expires_at,
            created_at=row.created_at,
            metadata=metadata or {},
        )
