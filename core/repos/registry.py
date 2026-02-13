"""Repository registry service."""

import json
from datetime import datetime, timezone

from uuid_utils import uuid7

from core.repos.models import AccessScope, OwnerType, Repo, RepoType
from sdk import Database


class RepoRegistry:
    """Repository registry for multi-repo management."""

    def __init__(self, db: Database | None = None) -> None:
        self.db = db or Database()

    def create(
        self,
        repo_url: str,
        repo_type: RepoType,
        owner_id: str,
        owner_type: OwnerType,
        access_scope: AccessScope,
        repo_group: str | None = None,
        token_id: str | None = None,
        metadata: dict | None = None,
    ) -> Repo:
        """Create a new repository."""
        repo_id = str(uuid7())
        now = datetime.now(timezone.utc)

        from sqlalchemy import text
        query = text("""
            INSERT INTO repos (
                repo_id, repo_url, repo_type, owner_id, owner_type,
                repo_group, token_id, access_scope, metadata, created_at, updated_at
            ) VALUES (:repo_id, :repo_url, :repo_type, :owner_id, :owner_type, 
                      :repo_group, :token_id, :access_scope, :metadata, :created_at, :updated_at)
        """)
        self.db.execute(
            query,
            {
                "repo_id": repo_id,
                "repo_url": repo_url,
                "repo_type": repo_type.value,
                "owner_id": owner_id,
                "owner_type": owner_type.value,
                "repo_group": repo_group,
                "token_id": token_id,
                "access_scope": access_scope.value,
                "metadata": json.dumps(metadata or {}),
                "created_at": now,
                "updated_at": now,
            },
        )
        self.db.commit()

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

    def get(self, repo_id: str) -> Repo | None:
        """Get repository by ID."""
        query = "SELECT * FROM repos WHERE repo_id = %s"
        result = self.db.fetchone(query, (repo_id,))
        if not result:
            return None
        return self._to_model(result)

    def get_by_url(self, repo_url: str, owner_id: str) -> Repo | None:
        """Get repository by URL and owner."""
        query = "SELECT * FROM repos WHERE repo_url = %s AND owner_id = %s"
        result = self.db.fetchone(query, (repo_url, owner_id))
        if not result:
            return None
        return self._to_model(result)

    def list_by_owner(self, owner_id: str, repo_type: RepoType | None = None) -> list[Repo]:
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
        from sqlalchemy import text
        query = text("""
            UPDATE repos
            SET token_id = :token_id, updated_at = :updated_at
            WHERE repo_id = :repo_id
        """)
        self.db.execute(query, {"token_id": token_id, "updated_at": datetime.now(timezone.utc), "repo_id": repo_id})
        self.db.commit()

    def update_metadata(self, repo_id: str, metadata: dict) -> None:
        """Update repository metadata."""
        from sqlalchemy import text
        query = text("""
            UPDATE repos
            SET metadata = :metadata, updated_at = :updated_at
            WHERE repo_id = :repo_id
        """)
        self.db.execute(query, {"metadata": json.dumps(metadata), "updated_at": datetime.now(timezone.utc), "repo_id": repo_id})
        self.db.commit()

    def deactivate(self, repo_id: str) -> None:
        """Deactivate repository."""
        from sqlalchemy import text
        query = text("""
            UPDATE repos
            SET is_active = FALSE, updated_at = :updated_at
            WHERE repo_id = :repo_id
        """)
        self.db.execute(query, {"updated_at": datetime.now(timezone.utc), "repo_id": repo_id})
        self.db.commit()

    def delete(self, repo_id: str) -> None:
        """Delete repository."""
        from sqlalchemy import text
        query = text("DELETE FROM repos WHERE repo_id = :repo_id")
        self.db.execute(query, {"repo_id": repo_id})
        self.db.commit()

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
