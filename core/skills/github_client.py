"""GitHub client wrapper for skills."""

import asyncio
import time

from github import Auth, Github, GithubException

from config.settings import get_settings
from core.exceptions import GitHubError, GitHubRateLimitError
from core.logging_config import get_logger
from sqlalchemy.orm import Session

settings = get_settings()
logger = get_logger(__name__)

RATE_LIMIT_THRESHOLD = 10  # pre-emptive pause when remaining < threshold


class GitHubClient:
    """Wrapper for GitHub API calls with error handling and rate limiting."""

    def __init__(self, session: Session | None = None, token: str | None = None, base_url: str | None = None):
        self._session = session
        self.token = token or settings.github_token

        # Auto-detect base_url from environment or use default
        if base_url is None:
            import os

            base_url = os.getenv("GITHUB_API_URL", "https://api.github.com")

        self.base_url = base_url

        if not self.token:
            logger.warning("GitHub token not configured, using unauthenticated access")
            self.client = Github(base_url=base_url)
        else:
            self.client = Github(auth=Auth.Token(token=self.token), base_url=base_url)

        logger.info(f"GitHub client initialized (base_url={base_url})")

    def _get_repo(self, repo_id: int):
        """Get repository from database and create GitHub repo object."""
        repo_data = None

        if not repo_data:
            # In test/dev mode, return mock repo URL
            if settings.is_development or settings.app_env == "test":
                logger.debug(f"Test mode: using mock repo for repo_id={repo_id}")
                repo_full_name = "octocat/Hello-World"  # GitHub's test repo
            else:
                raise GitHubError(f"Repository {repo_id} not found in database")
        else:
            # Extract owner/repo from URL
            # https://github.com/owner/repo -> owner/repo
            repo_url = repo_data["repo_url"]
            parts = repo_url.rstrip("/").split("/")
            repo_full_name = f"{parts[-2]}/{parts[-1]}"

        try:
            return self.client.get_repo(repo_full_name)
        except GithubException as e:
            if e.status == 404:
                raise GitHubError(
                    f"Repository {repo_full_name} not found on GitHub", status_code=404
                ) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            else:
                raise GitHubError(
                    f"GitHub API error: {e.data.get('message', str(e))}", status_code=e.status
                ) from e

    async def _check_rate_limit(self) -> None:
        """Pre-emptively wait if GitHub API rate limit is nearly exhausted."""
        now = time.monotonic()
        if now - getattr(self, "_rl_checked_at", 0) < 60:
            return
        try:
            rl = self.client.get_rate_limit()
            self._rl_checked_at = now
            core = getattr(rl, "core", None)
            if core and core.remaining < RATE_LIMIT_THRESHOLD:
                reset_ts = core.reset.timestamp() if core.reset else 0
                wait = max(0, reset_ts - time.time()) + 1
                if wait > 0:
                    logger.warning(
                        f"GitHub rate limit low ({core.remaining}/{core.limit}), "
                        f"waiting {wait:.0f}s until reset"
                    )
                    await asyncio.sleep(min(wait, 60))  # cap at 60s
        except Exception as exc:
            logger.debug(f"Rate limit check failed (non-fatal): {exc}")

    async def get_pr(self, repo_id: int, pr_number: int) -> dict:
        """Fetch PR details from GitHub.

        Args:
            repo_id: Repository ID
            pr_number: PR number

        Returns:
            PR details dict

        Raises:
            GitHubError: If API call fails
        """
        logger.info(f"Fetching PR #{pr_number} from repo {repo_id}")
        await self._check_rate_limit()

        try:
            repo = self._get_repo(repo_id)
            pr = repo.get_pull(pr_number)

            return {
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

        except GithubException as e:
            if e.status == 404:
                raise GitHubError(f"PR #{pr_number} not found", status_code=404) from e
            elif e.status == 403:
                raise GitHubRateLimitError() from e
            else:
                raise GitHubError(
                    f"Failed to fetch PR: {e.data.get('message', str(e))}", status_code=e.status
                ) from e

    async def get_pr_diff(self, repo_id: int, pr_number: int) -> str:
        """Fetch PR diff.

        Args:
            repo_id: Repository ID
            pr_number: PR number

        Returns:
            Diff string
        """
        logger.info(f"Fetching diff for PR #{pr_number} from repo {repo_id}")
        await self._check_rate_limit()

        try:
            repo = self._get_repo(repo_id)
            pr = repo.get_pull(pr_number)

            # Get files
            files = pr.get_files()
            diff_parts = []

            for file in files:
                if file.patch:
                    diff_parts.append(f"diff --git a/{file.filename} b/{file.filename}")
                    diff_parts.append(file.patch)

            return "\n".join(diff_parts)

        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to fetch diff: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def list_prs(self, repo_id: int, state: str = "open", limit: int = 10) -> list[dict]:
        """List PRs in a repo.

        Args:
            repo_id: Repository ID
            state: PR state (open, closed, all)
            limit: Maximum number of PRs

        Returns:
            List of PR dicts
        """
        logger.info(f"Listing PRs from repo {repo_id}, state={state}, limit={limit}")
        await self._check_rate_limit()

        try:
            repo = self._get_repo(repo_id)
            prs = repo.get_pulls(state=state)

            result = []
            for i, pr in enumerate(prs):
                if i >= limit:
                    break

                result.append(
                    {
                        "number": pr.number,
                        "title": pr.title,
                        "user": pr.user.login,
                        "state": pr.state,
                        "created_at": pr.created_at.isoformat(),
                        "html_url": pr.html_url,
                    }
                )

            return result

        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to list PRs: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    async def list_workflow_runs(self, repo_id: int, limit: int = 5) -> list[dict]:
        """List workflow runs.

        Args:
            repo_id: Repository ID
            limit: Maximum number of runs

        Returns:
            List of workflow run dicts
        """
        logger.info(f"Listing workflow runs from repo {repo_id}, limit={limit}")
        await self._check_rate_limit()

        try:
            repo = self._get_repo(repo_id)
            runs = repo.get_workflow_runs()

            result = []
            for i, run in enumerate(runs):
                if i >= limit:
                    break

                result.append(
                    {
                        "name": run.name or "Unnamed workflow",
                        "status": run.status,
                        "conclusion": run.conclusion,
                        "html_url": run.html_url,
                        "created_at": run.created_at.isoformat(),
                    }
                )

            return result

        except GithubException as e:
            if e.status == 403:
                raise GitHubRateLimitError() from e
            raise GitHubError(
                f"Failed to list workflows: {e.data.get('message', str(e))}", status_code=e.status
            ) from e

    def get_rate_limit(self) -> dict:
        """Get current rate limit status.

        Returns:
            Rate limit info dict
        """
        try:
            rate_limit = self.client.get_rate_limit()

            core_limit = getattr(rate_limit, "core", None)
            if not core_limit:
                return {"limit": 0, "remaining": 0, "reset": None}

            return {
                "limit": core_limit.limit,
                "remaining": core_limit.remaining,
                "reset": core_limit.reset.isoformat() if core_limit.reset else None,
            }
        except Exception as e:
            logger.error(f"Failed to get rate limit: {e}")
            return {"limit": 0, "remaining": 0, "reset": None}
