"""GitHub client wrapper for skills."""

from typing import Optional
from sdk import Database


class GitHubClient:
    """Wrapper for GitHub API calls"""

    def __init__(self, db: Database):
        self.db = db

    async def get_pr(self, repo_id: int, pr_number: int) -> dict:
        """Fetch PR details from GitHub"""
        # TODO: Implement actual GitHub API call
        # For now, return mock data
        return {
            "number": pr_number,
            "title": f"PR #{pr_number}",
            "body": "This is a test PR",
            "files_changed": 5,
            "additions": 120,
            "deletions": 30,
            "state": "open",
        }

    async def get_pr_diff(self, repo_id: int, pr_number: int) -> str:
        """Fetch PR diff"""
        # TODO: Implement actual GitHub API call
        return "diff --git a/file.py b/file.py\n+added line\n-removed line"

    async def list_prs(
        self, repo_id: int, state: str = "open", limit: int = 10
    ) -> list[dict]:
        """List PRs in a repo"""
        # TODO: Implement actual GitHub API call
        return [
            {
                "number": i,
                "title": f"PR #{i}",
                "user": {"login": "user"},
                "created_at": "2026-02-10T00:00:00Z",
                "html_url": f"https://github.com/owner/repo/pull/{i}",
            }
            for i in range(1, limit + 1)
        ]

    async def list_workflow_runs(self, repo_id: int, limit: int = 5) -> list[dict]:
        """List workflow runs"""
        # TODO: Implement actual GitHub API call
        return [
            {
                "name": f"Workflow {i}",
                "status": "completed",
                "conclusion": "success",
                "html_url": f"https://github.com/owner/repo/actions/runs/{i}",
                "created_at": "2026-02-10T00:00:00Z",
            }
            for i in range(1, limit + 1)
        ]
