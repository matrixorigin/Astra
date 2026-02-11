"""Repository registry service."""

import json
from datetime import UTC, datetime
from typing import Optional

from uuid_utils import uuid7

from core.repos.models import AccessScope, OwnerType, Repo, RepoType
from sdk import Database


class RepoRegistry:
    """Repository registry for multi-repo management."""

    def __init__(self, db: Optional[Database] = None) -> None:
        self.db = db or Database()

    def create(
        self,
        repo_url: str,
        repo_type: RepoType,
        owner_id: str,
        owner_type: OwnerType,
        access_scope: AccessScope,
        repo_group: Optional[str] = None,
        token_id: Optional[str] = None,
        metadata: Optional[dict] = None,
    ) -> Repo:
        """Create a new repository."""
        repo_id = str(uuid7())
        now = datetime.now(UTC)

        query = """
            INSERT INTO repos (
                repo_id, repo_url, repo_type, owner_id, owner_type,
                repo_group, token_id, access_scope, metadata, created_at, updated_at
            ) VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)
        """
        self.db.execute(
            query,
            (
                repo_id,
                repo_url,
                repo_type.value,
                owner_id,
                owner_type.value,
                repo_group,
                token_id,
                access_scope.value,
                json.dumps(metadata or {}),
                now,
                now,
            ),
        )

        return Repo(
            repo_id=repo_id,
            repo_url=repo_url,
            repo_type=repo_type,
            owner_id=owner_id,
            owner_type=owner_type,
            repo_group=repo_group,
            token_id=token_id,
            access_scope=access_scope,
            metadata=metadata or {},
            is_active=True,
            created_at=now,
            updated_at=now,
        )

    def get(self, repo_id: str) -> Optional[Repo]:
        """Get repository by ID."""
        query = "SELECT * FROM repos WHERE repo_id = %s"
        result = self.db.fetchone(query, (repo_id,))
        if not result:
            return None
        return self._to_model(result)

    def get_by_url(self, repo_url: str, owner_id: str) -> Optional[Repo]:
        """Get repository by URL and owner."""
        query = "SELECT * FROM repos WHERE repo_url = %s AND owner_id = %s"
        result = self.db.fetchone(query, (repo_url, owner_id))
        if not result:
            return None
        return self._to_model(result)

    def list_by_owner(self, owner_id: str, repo_type: Optional[RepoType] = None) -> list[Repo]:
        """List repositories by owner."""
        if repo_type:
            query = """
                SELECT * FROM repos 
                WHERE owner_id = %s AND repo_type = %s AND is_active = TRUE
                ORDER BY created_at DESC
            """
            results = self.db.fetchall(query, (owner_id, repo_type.value))
        else:
            query = """
                SELECT * FROM repos 
                WHERE owner_id = %s AND is_active = TRUE
                ORDER BY created_at DESC
            """
            results = self.db.fetchall(query, (owner_id,))

        return [self._to_model(r) for r in results]

    def list_by_group(self, repo_group: str) -> list[Repo]:
        """List repositories by group."""
        query = """
            SELECT * FROM repos 
            WHERE repo_group = %s AND is_active = TRUE
            ORDER BY repo_type, created_at DESC
        """
        results = self.db.fetchall(query, (repo_group,))
        return [self._to_model(r) for r in results]

    def update_token(self, repo_id: str, token_id: str) -> None:
        """Update repository token."""
        query = """
            UPDATE repos 
            SET token_id = %s, updated_at = %s
            WHERE repo_id = %s
        """
        self.db.execute(query, (token_id, datetime.now(UTC), repo_id))

    def update_metadata(self, repo_id: str, metadata: dict) -> None:
        """Update repository metadata."""
        query = """
            UPDATE repos 
            SET metadata = %s, updated_at = %s
            WHERE repo_id = %s
        """
        self.db.execute(query, (json.dumps(metadata), datetime.now(UTC), repo_id))

    def deactivate(self, repo_id: str) -> None:
        """Deactivate repository."""
        query = """
            UPDATE repos 
            SET is_active = FALSE, updated_at = %s
            WHERE repo_id = %s
        """
        self.db.execute(query, (datetime.now(UTC), repo_id))

    def delete(self, repo_id: str) -> None:
        """Delete repository."""
        query = "DELETE FROM repos WHERE repo_id = %s"
        self.db.execute(query, (repo_id,))

    def _to_model(self, row: dict) -> Repo:
        """Convert database row to Repo model."""
        metadata = row.get("metadata")
        if isinstance(metadata, str):
            metadata = json.loads(metadata)

        return Repo(
            repo_id=row["repo_id"],
            repo_url=row["repo_url"],
            repo_type=RepoType(row["repo_type"]),
            owner_id=row["owner_id"],
            owner_type=OwnerType(row["owner_type"]),
            repo_group=row.get("repo_group"),
            token_id=row.get("token_id"),
            access_scope=AccessScope(row["access_scope"]),
            metadata=metadata or {},
            is_active=row.get("is_active", True),
            created_at=row["created_at"],
            updated_at=row["updated_at"],
        )
