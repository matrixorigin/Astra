"""GitHub client wrapper — backward-compatible shim over GitHubSkillAPI.

DEPRECATED: Use skills.github.api.GitHubSkillAPI directly for new code.
This module exists only to keep existing callers (builtin.py, extended.py,
tests) working during migration. All methods delegate to GitHubSkillAPI.

Key change: repo_id: int parameter is IGNORED. The old _get_repo(repo_id)
was dead code that always fell back to "octocat/Hello-World". Now callers
must set self.default_repo to an "owner/repo" string, or pass repo_id
which is treated as a no-op (uses default_repo).
"""

from __future__ import annotations

import asyncio
import os
import time

from config.settings import get_settings
from core.logging_config import get_logger
from skills.github.api import GitHubSkillAPI
from sqlalchemy.orm import Session

settings = get_settings()
logger = get_logger(__name__)

RATE_LIMIT_THRESHOLD = 10  # re-exported for test compatibility


class GitHubClient:
    """Backward-compatible wrapper. Delegates to GitHubSkillAPI.

    DEPRECATED — use GitHubSkillAPI directly for new code.
    """

    def __init__(
        self,
        session: Session | None = None,
        token: str | None = None,
        base_url: str | None = None,
    ):
        self._session = session
        self.token = token or settings.github_token
        if base_url is None:
            base_url = os.getenv("GITHUB_API_URL", "https://api.github.com")
        self.base_url = base_url
        self.default_repo = "octocat/Hello-World"  # legacy fallback

        self._api = GitHubSkillAPI(
            token=self.token,
            base_url=base_url,
        )
        # Expose PyGithub client for backward compat (tests set gh.client directly)
        self.client = self._api._client
        self._rl_checked_at = 0
        logger.info(f"GitHubClient initialized (base_url={base_url}) [deprecated shim]")

    def _get_repo(self, repo_id: int):
        """DEPRECATED — repo_id is ignored, uses default_repo."""
        return self._api._get_repo(self.default_repo)

    async def _check_rate_limit(self) -> None:
        if hasattr(self, "_api"):
            await self._api._check_rate_limit()
        else:
            # Backward compat: tests using object.__new__ set self.client directly
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
                        await asyncio.sleep(min(wait, 60))
            except Exception:
                pass

    async def get_pr(self, repo_id: int, pr_number: int) -> dict:
        return await self._api.get_pr(self.default_repo, pr_number)

    async def get_pr_diff(self, repo_id: int, pr_number: int) -> str:
        return await self._api.get_pr_diff(self.default_repo, pr_number)

    async def list_prs(self, repo_id: int, state: str = "open", limit: int = 10) -> list[dict]:
        return await self._api.list_prs(self.default_repo, state, limit)

    async def list_wf_runs(self, repo_id: int, limit: int = 5) -> list[dict]:
        return await self._api.list_wf_runs(self.default_repo, limit)

    def get_rate_limit(self) -> dict:
        return self._api.get_rate_limit()
