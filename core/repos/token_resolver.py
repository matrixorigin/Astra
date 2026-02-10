"""Token resolution service.

Implements the token resolution priority from design doc:
1. Repo-specific token
2. User default token
3. Tenant default token
4. Global fallback (if allowed)
"""

import json
from datetime import UTC, datetime
from typing import Optional

from ulid import ULID

from core.repos.token_models import Token, TokenType
from sdk import Database


class TokenResolver:
    """Token resolution service."""

    def __init__(self, db: Optional[Database] = None) -> None:
        self.db = db or Database()

    def resolve_repo_token(
        self,
        user_id: str,
        tenant_id: Optional[str] = None,
        repo_url: Optional[str] = None,
        repo_id: Optional[str] = None,
    ) -> Optional[Token]:
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
        secret_ref: Optional[str] = None,
        encrypted_value: Optional[str] = None,
        scope_user_id: Optional[str] = None,
        scope_tenant_id: Optional[str] = None,
        scope_repo: Optional[str] = None,
        expires_at: Optional[datetime] = None,
        metadata: Optional[dict] = None,
    ) -> Token:
        """Create a new token."""
        token_id = str(ULID())
        now = datetime.now(UTC)

        query = """
            INSERT INTO tokens (
                token_id, type, provider, scope_user_id, scope_tenant_id,
                scope_repo, secret_ref, encrypted_value, is_active,
                expires_at, created_at, metadata
            ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
        """
        self.db.execute(
            query,
            (
                token_id,
                token_type.value,
                provider,
                scope_user_id,
                scope_tenant_id,
                scope_repo,
                secret_ref,
                encrypted_value,
                True,
                expires_at,
                now,
                json.dumps(metadata or {}),
            ),
        )

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

    def get_token(self, token_id: str) -> Optional[Token]:
        """Get token by ID."""
        query = "SELECT * FROM tokens WHERE token_id = %s"
        result = self.db.fetchone(query, (token_id,))
        if not result:
            return None
        return self._to_model(result)

    def deactivate_token(self, token_id: str) -> None:
        """Deactivate token (e.g., on 401 error)."""
        query = "UPDATE tokens SET is_active = FALSE WHERE token_id = %s"
        self.db.execute(query, (token_id,))

    def _get_repo_specific_token(
        self, repo_id: Optional[str], repo_url: Optional[str], user_id: str
    ) -> Optional[Token]:
        """Get repo-specific token from repos table."""
        if repo_id:
            query = """
                SELECT t.* FROM tokens t
                JOIN repos r ON r.token_id = t.token_id
                WHERE r.repo_id = %s AND r.owner_id = %s AND t.is_active = TRUE
            """
            result = self.db.fetchone(query, (repo_id, user_id))
        elif repo_url:
            query = """
                SELECT t.* FROM tokens t
                JOIN repos r ON r.token_id = t.token_id
                WHERE r.repo_url = %s AND r.owner_id = %s AND t.is_active = TRUE
            """
            result = self.db.fetchone(query, (repo_url, user_id))
        else:
            return None

        return self._to_model(result) if result else None

    def _get_user_default_token(self, user_id: str) -> Optional[Token]:
        """Get user default token (no scope_repo)."""
        query = """
            SELECT * FROM tokens
            WHERE type = 'repo'
              AND scope_user_id = %s
              AND scope_repo IS NULL
              AND is_active = TRUE
            ORDER BY created_at DESC
            LIMIT 1
        """
        result = self.db.fetchone(query, (user_id,))
        return self._to_model(result) if result else None

    def _get_tenant_default_token(self, tenant_id: str) -> Optional[Token]:
        """Get tenant default token (no scope_repo)."""
        query = """
            SELECT * FROM tokens
            WHERE type = 'repo'
              AND scope_tenant_id = %s
              AND scope_repo IS NULL
              AND is_active = TRUE
            ORDER BY created_at DESC
            LIMIT 1
        """
        result = self.db.fetchone(query, (tenant_id,))
        return self._to_model(result) if result else None

    def _get_global_token(self) -> Optional[Token]:
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
        result = self.db.fetchone(query)
        return self._to_model(result) if result else None

    def _allow_global_token(self) -> bool:
        """Check if global token fallback is allowed."""
        query = """
            SELECT value FROM configs
            WHERE key_name = 'allow_global_repo_token'
            LIMIT 1
        """
        result = self.db.fetchone(query)
        if not result:
            return False
        return result["value"].lower() in ("true", "1", "yes")

    def _to_model(self, row: dict) -> Token:
        """Convert database row to Token model."""
        metadata = row.get("metadata")
        if isinstance(metadata, str):
            metadata = json.loads(metadata)

        return Token(
            token_id=row["token_id"],
            token_type=TokenType(row["type"]),
            provider=row.get("provider"),
            scope_user_id=row.get("scope_user_id"),
            scope_tenant_id=row.get("scope_tenant_id"),
            scope_repo=row.get("scope_repo"),
            secret_ref=row.get("secret_ref"),
            encrypted_value=row.get("encrypted_value"),
            is_active=row.get("is_active", True),
            expires_at=row.get("expires_at"),
            created_at=row["created_at"],
            metadata=metadata or {},
        )
