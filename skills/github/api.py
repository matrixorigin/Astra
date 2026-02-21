"""GitHub skill API — typed interface for data access.

Replaces the old GitHubClient. Key changes:
- repo identified by "owner/repo" string, not repo_id: int
- PR cache in platform DB (sk_github_ prefix tables)
- Credentials from platform credential manager
"""

from __future__ import annotations

import asyncio
import time
import uuid
from datetime import datetime, timezone

from github import Auth, Github, GithubException
from sqlalchemy.orm import Session

from core.exceptions import GitHubError, GitHubRateLimitError
from core.logging_config import get_logger
from skills.github.models import SkGithubPRCache, SkGithubRepo

logger = get_logger(__name__)

RATE_LIMIT_THRESHOLD = 10


class GitHubSkillAPI:
    """Typed API for GitHub skill. Users interact through this, not direct SQL."""

    def __init__(
        self,
        token: str | None = None,
        db: Session | None = None,
        base_url: str = "https://api.github.com",
    ):
        self._db = db
        self._base_url = base_url
        self._rl_checked_at: float = 0

        if token:
            self._client = Github(auth=Auth.Token(token=token), base_url=base_url)
        else:
            logger.warning("GitHub token not configured, using unauthenticated access")
            self._client = Github(base_url=base_url)

    # ------------------------------------------------------------------
    # Repo management
    # ------------------------------------------------------------------

    def add_repo(self, owner: str, name: str) -> dict:
        """Register a repository for tracking."""
        if not self._db:
            raise RuntimeError("DB session required for repo management")
        full_name = f"{owner}/{name}"
        existing = (
            self._db.query(SkGithubRepo)
            .filter_by(owner=owner, name=name)
            .one_or_none()
        )
        if existing:
            return {"repo_id": existing.repo_id, "full_name": full_name, "created": False}
        repo = SkGithubRepo(
            repo_id=str(uuid.uuid4()),
            owner=owner,
            name=name,
            full_name=full_name,
        )
        self._db.add(repo)
        self._db.flush()
        return {"repo_id": repo.repo_id, "full_name": full_name, "created": True}

    def list_repos(self) -> list[dict]:
        """List registered repositories."""
        if not self._db:
            return []
        rows = self._db.query(SkGithubRepo).all()
        return [
            {"repo_id": r.repo_id, "full_name": r.full_name, "default_branch": r.default_branch}
            for r in rows
        ]

    # ------------------------------------------------------------------
    # GitHub API helpers
    # ------------------------------------------------------------------

    def _get_repo(self, repo: str):
        """Get PyGithub repo object by 'owner/repo' string."""
        try:
            return self._client.get_repo(repo)
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"Repository {repo} not found on GitHub", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"GitHub API error: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def _check_rate_limit(self) -> None:
        """Pre-emptively wait if rate limit is nearly exhausted."""
        now = time.monotonic()
        if now - self._rl_checked_at < 60:
            return
        try:
            rl = self._client.get_rate_limit()
            self._rl_checked_at = now
            core = getattr(rl, "core", None)
            if core and core.remaining < RATE_LIMIT_THRESHOLD:
                reset_ts = core.reset.timestamp() if core.reset else 0
                wait = max(0, reset_ts - time.time()) + 1
                if wait > 0:
                    logger.warning(
                        f"GitHub rate limit low ({core.remaining}/{core.limit}), "
                        f"waiting {wait:.0f}s"
                    )
                    await asyncio.sleep(min(wait, 60))
        except Exception as exc:
            logger.debug(f"Rate limit check failed (non-fatal): {exc}")

    # ------------------------------------------------------------------
    # PR operations
    # ------------------------------------------------------------------

    async def get_pr(self, repo: str, pr_number: int) -> dict:
        """Fetch PR details. Caches in user DB if available."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            pr = gh_repo.get_pull(pr_number)
            result = {
                "number": pr.number,
                "title": pr.title,
                "body": pr.body or "",
                "state": pr.state,
                "files_changed": pr.changed_files,
                "additions": pr.additions,
                "deletions": pr.deletions,
                "user": pr.user.login,
                "created_at": pr.created_at.isoformat(),
                "updated_at": pr.updated_at.isoformat(),
                "html_url": pr.html_url,
            }
            self._cache_pr(repo, result)
            return result
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"PR #{pr_number} not found", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to fetch PR: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def get_pr_diff(self, repo: str, pr_number: int) -> str:
        """Fetch PR diff."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            pr = gh_repo.get_pull(pr_number)
            parts = []
            for f in pr.get_files():
                if f.patch:
                    parts.append(f"diff --git a/{f.filename} b/{f.filename}")
                    parts.append(f.patch)
            return "\n".join(parts)
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to fetch diff: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def list_prs(self, repo: str, state: str = "open", limit: int = 10) -> list[dict]:
        """List PRs in a repo."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            prs = gh_repo.get_pulls(state=state)
            result = []
            for i, pr in enumerate(prs):
                if i >= limit:
                    break
                item = {
                    "number": pr.number,
                    "title": pr.title,
                    "user": pr.user.login,
                    "state": pr.state,
                    "created_at": pr.created_at.isoformat(),
                    "html_url": pr.html_url,
                }
                result.append(item)
            return result
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to list PRs: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def get_pr_checks(self, repo: str, pr_number: int) -> dict:
        """Get CI/check run status for a specific PR."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            pr = gh_repo.get_pull(pr_number)
            commit = gh_repo.get_commit(pr.head.sha)
            check_runs = commit.get_check_runs()
            runs = []
            for cr in check_runs:
                runs.append({
                    "name": cr.name,
                    "status": cr.status,
                    "conclusion": cr.conclusion,
                    "html_url": cr.html_url,
                    "started_at": cr.started_at.isoformat() if cr.started_at else None,
                    "completed_at": cr.completed_at.isoformat() if cr.completed_at else None,
                })
            # Overall status
            overall = "success"
            for r in runs:
                if r["conclusion"] == "failure":
                    overall = "failure"
                    break
                if r["status"] != "completed":
                    overall = "pending"
            return {"pr_number": pr_number, "overall": overall, "check_runs": runs}
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"PR #{pr_number} not found", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to get check runs: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    # ------------------------------------------------------------------
    # Workflow runs
    # ------------------------------------------------------------------

    async def list_workflow_runs(self, repo: str, limit: int = 5) -> list[dict]:
        """List workflow runs."""
        await self._check_rate_limit()
        try:
            gh_repo = self._get_repo(repo)
            runs = gh_repo.get_workflow_runs()
            result = []
            for i, run in enumerate(runs):
                if i >= limit:
                    break
                result.append({
                    "name": run.name or "Unnamed workflow",
                    "status": run.status,
                    "conclusion": run.conclusion,
                    "html_url": run.html_url,
                    "created_at": run.created_at.isoformat(),
                })
            return result
        except (GitHubError, GitHubRateLimitError):
            raise
        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to list workflows: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    # ------------------------------------------------------------------
    # Rate limit
    # ------------------------------------------------------------------

    def get_rate_limit(self) -> dict:
        """Get current rate limit status."""
        try:
            rl = self._client.get_rate_limit()
            core = getattr(rl, "core", None)
            if not core:
                return {"limit": 0, "remaining": 0, "reset": None}
            return {
                "limit": core.limit,
                "remaining": core.remaining,
                "reset": core.reset.isoformat() if core.reset else None,
            }
        except Exception as e:
            logger.error(f"Failed to get rate limit: {e}")
            return {"limit": 0, "remaining": 0, "reset": None}

    # ------------------------------------------------------------------
    # Cache helpers
    # ------------------------------------------------------------------

    def _cache_pr(self, repo: str, pr_data: dict) -> None:
        """Upsert PR data into cache (if DB session available)."""
        if not self._db:
            return
        try:
            existing = (
                self._db.query(SkGithubPRCache)
                .filter_by(repo_full_name=repo, pr_number=pr_data["number"])
                .one_or_none()
            )
            now = datetime.now(timezone.utc)
            if existing:
                existing.title = pr_data.get("title")
                existing.state = pr_data.get("state")
                existing.author = pr_data.get("user")
                existing.data = pr_data
                existing.fetched_at = now
            else:
                self._db.add(SkGithubPRCache(
                    cache_id=str(uuid.uuid4()),
                    repo_full_name=repo,
                    pr_number=pr_data["number"],
                    title=pr_data.get("title"),
                    state=pr_data.get("state"),
                    author=pr_data.get("user"),
                    data=pr_data,
                    fetched_at=now,
                ))
            self._db.flush()
        except Exception as e:
            logger.debug(f"PR cache upsert failed (non-fatal): {e}")
