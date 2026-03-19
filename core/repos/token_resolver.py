"""Token resolution service.

Implements the token resolution priority from design doc:
1. Repo-specific token
2. User default token
3. Global fallback (if allowed)
"""

import json
import time
from datetime import datetime, timezone

from sqlalchemy import text
from sqlalchemy.engine import Connection, Engine
from sqlalchemy.orm import sessionmaker
from uuid_utils import uuid7

from api.database import get_db_session
from core.repos.token_models import Token, TokenType
from core.db_consumer import DbConsumer, DbFactory


class TokenResolver(DbConsumer):
    """Token resolution service."""

    def __init__(self, db_factory: DbFactory) -> None:
        super().__init__(db_factory)

    def resolve_repo_token(
        self,
        user_id: str,
        repo_url: str | None = None,
        repo_id: str | None = None,
    ) -> Token | None:
        """Resolve repo token with priority fallback.

        Priority:
        1. Repo-specific token (from infra_repos.token_id)
        2. User default token (scope_user_id, no scope_repo)
        3. Global fallback (if config allows)
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

        # 3. Global fallback (check config)
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
        scope_repo: str | None = None,
        expires_at: datetime | None = None,
        metadata: dict | None = None,
    ) -> Token:
        """Create a new token."""
        with self._db() as db:
            from api.models import Token as TokenModel

            token_id = str(uuid7())
            now = datetime.now(timezone.utc)

            token_model = TokenModel(
                token_id=token_id,
                type=token_type.value,
                provider=provider,
                scope_user_id=scope_user_id,
                scope_repo=scope_repo,
                secret_ref=secret_ref,
                encrypted_value=encrypted_value,
                is_active=True,
                expires_at=expires_at,
                created_at=now,
                token_metadata=metadata or {},
            )
            db.add(token_model)
            db.commit()
            bind = db.get_bind()
            if isinstance(bind, (Engine, Connection)):
                fresh_factory = sessionmaker(bind=bind, expire_on_commit=False)
                for attempt in range(6):
                    fresh_db = fresh_factory()
                    try:
                        visible = (
                            fresh_db.query(TokenModel)
                            .filter(TokenModel.token_id == token_id)
                            .first()
                        )
                    finally:
                        fresh_db.close()
                    if visible is not None:
                        break
                    if attempt < 5:
                        time.sleep(0.03 * (attempt + 1))

            return Token(
                token_id=token_id,
                token_type=token_type,
                provider=provider,
                scope_user_id=scope_user_id,
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
        with self._db() as db:
            from api.models import Token as TokenModel

            result = db.query(TokenModel).filter(TokenModel.token_id == token_id).first()
            if not result:
                return None
            return self._to_model(result)

    def deactivate_token(self, token_id: str) -> None:
        """Deactivate token (e.g., on 401 error)."""
        with self._db() as db:
            from api.models import Token as TokenModel

            db.query(TokenModel).filter(TokenModel.token_id == token_id).update(
                {"is_active": False}
            )
            db.commit()
            db.expire_all()  # Clear session cache
            bind = db.get_bind()
            if isinstance(bind, (Engine, Connection)):
                fresh_factory = sessionmaker(bind=bind, expire_on_commit=False)
                for attempt in range(6):
                    fresh_db = fresh_factory()
                    try:
                        row = fresh_db.query(TokenModel).filter(TokenModel.token_id == token_id).first()
                    finally:
                        fresh_db.close()
                    if row is None or not row.is_active:
                        break
                    if attempt < 5:
                        time.sleep(0.03 * (attempt + 1))

    def _get_repo_specific_token(
        self, repo_id: str | None, repo_url: str | None, user_id: str
    ) -> Token | None:
        """Get repo-specific token from infra_repos table."""
        with self._db() as db:
            from api.models import Repo as RepoModel
            from api.models import Token as TokenModel

            query = (
                db.query(TokenModel)
                .join(RepoModel, RepoModel.token_id == TokenModel.token_id)
                .filter(TokenModel.is_active == 1)
            )

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
        with self._db() as db:
            from api.models import Token as TokenModel

            result = (
                db.query(TokenModel)
                .filter(
                    TokenModel.type == "repo",
                    TokenModel.scope_user_id == user_id,
                    TokenModel.scope_repo.is_(None),
                    TokenModel.is_active == 1,
                )
                .order_by(TokenModel.created_at.desc())
                .first()
            )
            return self._to_model(result) if result else None

    def _get_global_token(self) -> Token | None:
        """Get global fallback token (no user, no repo scope)."""
        with self._db() as db:
            from api.models import Token as TokenModel

            result = (
                db.query(TokenModel)
                .filter(
                    TokenModel.type == "repo",
                    TokenModel.scope_user_id.is_(None),
                    TokenModel.scope_repo.is_(None),
                    TokenModel.is_active == 1,
                )
                .order_by(TokenModel.created_at.desc())
                .first()
            )
            return self._to_model(result) if result else None

    def _allow_global_token(self) -> bool:
        """Check if global token fallback is allowed using ORM."""

        with self._db() as db:
            from api.models import Config

            result = (
                db.query(Config.value).filter(Config.key_name == "allow_global_repo_token").first()
            )

            if not result:
                return False
            return result[0].lower() in ("true", "1", "yes")

    def _to_model(self, row) -> Token:
        """Convert ORM object to Token model.

        Automatically decrypts encrypted_value if present.
        """
        from core.auth.encryption import decrypt_token

        metadata = getattr(row, "token_metadata", None)
        if isinstance(metadata, str):
            metadata = json.loads(metadata)

        # Decrypt encrypted_value if present
        decrypted_value = None
        if row.encrypted_value:
            decrypted_value = decrypt_token(row.encrypted_value)

        return Token(
            token_id=row.token_id,
            token_type=TokenType(row.type),
            provider=row.provider,
            scope_user_id=row.scope_user_id,
            scope_repo=row.scope_repo,
            secret_ref=row.secret_ref,
            encrypted_value=decrypted_value,  # Return decrypted value
            is_active=bool(row.is_active),
            expires_at=row.expires_at,
            created_at=row.created_at,
            metadata=metadata or {},
        )
