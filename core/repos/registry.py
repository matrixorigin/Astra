"""Repository registry service."""

import logging
from datetime import datetime, timezone

from uuid_utils import uuid7

from sqlalchemy import text

from api.database import get_db_session
from core.repos.models import AccessScope, OwnerType, Repo, RepoType
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)


class RepoRegistry(DbConsumer):
    """Repository registry for multi-repo management."""

    def __init__(self, db_factory: DbFactory) -> None:
        super().__init__(db_factory)

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
        with self._db() as db:
            from api.models import Repo as RepoModel
        
            repo_id = str(uuid7())
            now = datetime.now(timezone.utc)
        
            # Derive repo name from URL
            repo_name = repo_url.rstrip("/").split("/")[-1]
        
            # Prepare metadata (include repo_group if present)
            final_metadata = metadata or {}
            if repo_group:
                final_metadata["repo_group"] = repo_group

            repo_model = RepoModel(
                repo_id=repo_id,
                user_id=owner_id,  # Map owner_id to user_id
                repo_url=repo_url,
                repo_name=repo_name,
                repo_type=repo_type.value,
                token_id=token_id,
                access_scope=access_scope.value,
                branch=final_metadata.get("default_branch", "main"),
                status="active",
                repo_metadata=final_metadata,
                created_at=now,
                updated_at=now,
            )
        
            db.add(repo_model)
            db.commit()
            db.refresh(repo_model)

            return self._to_model(repo_model)

    def get(self, repo_id: str) -> Repo | None:
        """Get repository by ID."""
        with self._db() as db:
            from api.models import Repo as RepoModel
            result = db.query(RepoModel).filter(RepoModel.repo_id == repo_id).first()
            if not result:
                return None
            return self._to_model(result)

    def get_by_url(self, repo_url: str, owner_id: str) -> Repo | None:
        """Get repository by URL and owner."""
        with self._db() as db:
            from api.models import Repo as RepoModel
            result = db.query(RepoModel).filter(
                RepoModel.repo_url == repo_url,
                RepoModel.user_id == owner_id  # Map owner_id to user_id
            ).first()
            if not result:
                return None
            return self._to_model(result)

    def list_by_owner(self, owner_id: str, repo_type: RepoType | None = None) -> list[Repo]:
        """List repositories by owner.

        Returns at most 200 repos. Logs a warning if truncated.
        """
        max_results = 200
        with self._db() as db:
            from api.models import Repo as RepoModel
            query = db.query(RepoModel).filter(
                RepoModel.user_id == owner_id,  # Map owner_id to user_id
                RepoModel.status == "active"
            )
        
            if repo_type:
                query = query.filter(RepoModel.repo_type == repo_type.value)
        
            results = query.order_by(RepoModel.created_at.desc()).limit(max_results).all()
            if len(results) >= max_results:
                logger.warning("list_by_owner(%s) hit limit of %d; results truncated", owner_id, max_results)
            return [self._to_model(r) for r in results]

    def list_by_group(self, repo_group: str) -> list[Repo]:
        """List repositories by group.

        Uses JSON_EXTRACT to filter at the DB level so the limit does not
        silently discard matching rows that happen to sit beyond the cap.
        """
        with self._db() as db:
            from api.models import Repo as RepoModel
        
            results = db.query(RepoModel).filter(
                RepoModel.status == "active",
                text("JSON_UNQUOTE(JSON_EXTRACT(repo_metadata, '$.repo_group')) = :rg"),
            ).params(rg=repo_group).limit(500).all()
        
            return [self._to_model(r) for r in results]

    def update_token(self, repo_id: str, token_id: str) -> None:
        """Update repository token."""
        with self._db() as db:
            from api.models import Repo as RepoModel
            repo = db.query(RepoModel).filter(RepoModel.repo_id == repo_id).first()
            if repo:
                repo.token_id = token_id
                repo.updated_at = datetime.now(timezone.utc)
                db.commit()

    def update_metadata(self, repo_id: str, metadata: dict) -> None:
        """Update repository metadata."""
        with self._db() as db:
            from api.models import Repo as RepoModel
            from sqlalchemy.orm.attributes import flag_modified
        
            repo = db.query(RepoModel).filter(RepoModel.repo_id == repo_id).first()
            if repo:
                # Create a copy to ensure SQLAlchemy detects the change
                current = dict(repo.repo_metadata or {})
                current.update(metadata)
                repo.repo_metadata = current
                # Explicitly flag as modified to be safe with JSON types
                flag_modified(repo, "repo_metadata")
                repo.updated_at = datetime.now(timezone.utc)
                db.commit()

    def deactivate(self, repo_id: str) -> None:
        """Deactivate repository."""
        with self._db() as db:
            from api.models import Repo as RepoModel
            repo = db.query(RepoModel).filter(RepoModel.repo_id == repo_id).first()
            if repo:
                repo.status = "inactive"
                repo.updated_at = datetime.now(timezone.utc)
                db.commit()

    def delete(self, repo_id: str) -> None:
        """Delete repository."""
        with self._db() as db:
            from api.models import Repo as RepoModel
            db.query(RepoModel).filter(RepoModel.repo_id == repo_id).delete()
            db.commit()

    def _to_model(self, row) -> Repo:
        """Convert database row to Repo model."""
        # Row is an SQLAlchemy model instance (RepoModel)
        
        metadata = row.repo_metadata or {}
        repo_group = metadata.get("repo_group")
        
        return Repo(
            repo_id=row.repo_id,
            repo_url=row.repo_url,
            repo_type=RepoType(row.repo_type) if row.repo_type else RepoType.CODE,
            owner_id=row.user_id,
            owner_type=OwnerType.USER, # Hardcoded as DB only has user_id
            repo_group=repo_group,
            token_id=row.token_id,
            access_scope=AccessScope(row.access_scope) if row.access_scope else AccessScope.READ,
            metadata=metadata,
            is_active=(row.status == "active"),
            created_at=row.created_at,
            updated_at=row.updated_at,
        )
