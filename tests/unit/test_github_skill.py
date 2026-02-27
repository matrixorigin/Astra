"""Tests for GitHub skill — api.py + actions.py.

v3: Uses platform DB session (sk_github_* tables). GitHub API calls are mocked.
"""

import uuid
from datetime import datetime, timezone
from unittest.mock import AsyncMock, MagicMock

import pytest

from skills.github.api import GitHubSkillAPI
from skills.github.models import SkGithubPRCache, SkGithubRepo


# ── Repo management (real DB) ────────────────────────────────────────────────


class TestRepoManagement:
    @pytest.fixture
    def api(self, db_session):
        return GitHubSkillAPI(token=None, db=db_session)

    def test_add_repo(self, api):
        result = api.add_repo("octocat", f"repo_{uuid.uuid4().hex[:8]}")
        assert result["created"] is True
        assert result["repo_id"]

    def test_add_repo_idempotent(self, api):
        name = f"repo_{uuid.uuid4().hex[:8]}"
        r1 = api.add_repo("octocat", name)
        r2 = api.add_repo("octocat", name)
        assert r1["repo_id"] == r2["repo_id"]
        assert r2["created"] is False

    def test_list_repos(self, api):
        name = f"repo_{uuid.uuid4().hex[:8]}"
        api.add_repo("testowner", name)
        repos = api.list_repos()
        names = [r["full_name"] for r in repos]
        assert f"testowner/{name}" in names

    def test_add_repo_no_db(self):
        api = GitHubSkillAPI(token=None, db=None)
        with pytest.raises(RuntimeError, match="DB session required"):
            api.add_repo("x", "y")

    def test_list_repos_no_db(self):
        api = GitHubSkillAPI(token=None, db=None)
        assert api.list_repos() == []


# ── PR cache (real DB) ───────────────────────────────────────────────────────


class TestPRCache:
    @pytest.fixture
    def api(self, db_session):
        return GitHubSkillAPI(token=None, db=db_session)

    def test_cache_pr_insert(self, api, db_session):
        repo = f"owner/cache_test_{uuid.uuid4().hex[:8]}"
        api._cache_pr(repo, {"number": 42, "title": "Test PR", "state": "open", "user": "alice"})
        row = db_session.query(SkGithubPRCache).filter_by(repo_full_name=repo, pr_number=42).one()
        assert row.title == "Test PR"
        assert row.state == "open"

    def test_cache_pr_update(self, api, db_session):
        repo = f"owner/cache_upd_{uuid.uuid4().hex[:8]}"
        api._cache_pr(repo, {"number": 1, "title": "v1", "state": "open", "user": "a"})
        api._cache_pr(repo, {"number": 1, "title": "v2", "state": "closed", "user": "a"})
        row = db_session.query(SkGithubPRCache).filter_by(repo_full_name=repo, pr_number=1).one()
        assert row.title == "v2"
        assert row.state == "closed"


# ── GitHub API methods (mocked GitHub, real cache) ───────────────────────────


class TestGitHubAPIMethods:
    @pytest.fixture
    def api(self, db_session):
        return GitHubSkillAPI(token=None, db=db_session)

    @pytest.fixture
    def mock_gh_client(self, api):
        mock_client = MagicMock()
        api._client = mock_client
        return mock_client

    def _make_mock_pr(self, number=1, title="Test", state="open"):
        pr = MagicMock()
        pr.number = number
        pr.title = title
        pr.body = "body"
        pr.state = state
        pr.changed_files = 3
        pr.additions = 10
        pr.deletions = 5
        pr.user.login = "alice"
        pr.created_at = datetime(2026, 1, 1, tzinfo=timezone.utc)
        pr.updated_at = datetime(2026, 1, 2, tzinfo=timezone.utc)
        pr.html_url = f"https://github.com/o/r/pull/{number}"
        pr.head.sha = "abc123"
        return pr

    @pytest.mark.asyncio
    async def test_get_pr(self, api, mock_gh_client, db_session):
        mock_repo = MagicMock()
        mock_repo.get_pull.return_value = self._make_mock_pr(42, "My PR")
        mock_gh_client.get_repo.return_value = mock_repo

        result = await api.get_pr("owner/repo", 42)
        assert result["number"] == 42
        assert result["title"] == "My PR"

        cached = db_session.query(SkGithubPRCache).filter_by(
            repo_full_name="owner/repo", pr_number=42
        ).one_or_none()
        assert cached is not None
        assert cached.title == "My PR"

    @pytest.mark.asyncio
    async def test_list_prs(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_pulls.return_value = [
            self._make_mock_pr(1, "PR 1"),
            self._make_mock_pr(2, "PR 2"),
        ]
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.list_prs("owner/repo", state="open", limit=10)
        assert len(result) == 2

    @pytest.mark.asyncio
    async def test_get_pr_diff(self, api, mock_gh_client):
        mock_file = MagicMock()
        mock_file.filename = "README.md"
        mock_file.patch = "+hello"
        mock_pr = self._make_mock_pr()
        mock_pr.get_files.return_value = [mock_file]
        mock_repo = MagicMock()
        mock_repo.get_pull.return_value = mock_pr
        mock_gh_client.get_repo.return_value = mock_repo
        diff = await api.get_pr_diff("owner/repo", 1)
        assert "README.md" in diff

    @pytest.mark.asyncio
    async def test_get_pr_checks(self, api, mock_gh_client):
        mock_cr = MagicMock()
        mock_cr.name = "CI"
        mock_cr.status = "completed"
        mock_cr.conclusion = "success"
        mock_cr.html_url = "https://github.com/o/r/runs/1"
        mock_cr.started_at = datetime(2026, 1, 1, tzinfo=timezone.utc)
        mock_cr.completed_at = datetime(2026, 1, 1, tzinfo=timezone.utc)
        mock_commit = MagicMock()
        mock_commit.get_check_runs.return_value = [mock_cr]
        mock_pr = self._make_mock_pr()
        mock_repo = MagicMock()
        mock_repo.get_pull.return_value = mock_pr
        mock_repo.get_commit.return_value = mock_commit
        mock_gh_client.get_repo.return_value = mock_repo

        result = await api.get_pr_checks("owner/repo", 1)
        assert result["overall"] == "success"
        assert len(result["check_runs"]) == 1

    @pytest.mark.asyncio
    async def test_get_pr_checks_failure(self, api, mock_gh_client):
        mock_cr = MagicMock()
        mock_cr.name = "CI"
        mock_cr.status = "completed"
        mock_cr.conclusion = "failure"
        mock_cr.html_url = "url"
        mock_cr.started_at = None
        mock_cr.completed_at = None
        mock_commit = MagicMock()
        mock_commit.get_check_runs.return_value = [mock_cr]
        mock_pr = self._make_mock_pr()
        mock_repo = MagicMock()
        mock_repo.get_pull.return_value = mock_pr
        mock_repo.get_commit.return_value = mock_commit
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.get_pr_checks("owner/repo", 1)
        assert result["overall"] == "failure"

    @pytest.mark.asyncio
    async def test_list_wf_runs(self, api, mock_gh_client):
        mock_run = MagicMock()
        mock_run.name = "Build"
        mock_run.status = "completed"
        mock_run.conclusion = "success"
        mock_run.html_url = "url"
        mock_run.created_at = datetime(2026, 1, 1, tzinfo=timezone.utc)
        mock_repo = MagicMock()
        mock_repo.get_wf_runs.return_value = [mock_run]
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.list_wf_runs("owner/repo", limit=5)
        assert len(result) == 1
        assert result[0]["name"] == "Build"

    def test_get_rate_limit(self, api, mock_gh_client):
        mock_core = MagicMock()
        mock_core.limit = 5000
        mock_core.remaining = 4999
        mock_core.reset = datetime(2026, 1, 1, tzinfo=timezone.utc)
        mock_rl = MagicMock()
        mock_rl.core = mock_core
        mock_gh_client.get_rate_limit.return_value = mock_rl
        result = api.get_rate_limit()
        assert result["limit"] == 5000


# ── Actions ──────────────────────────────────────────────────────────────────


class TestActions:
    @pytest.mark.asyncio
    async def test_list_prs_action(self):
        from skills.github.actions import ListPRsAction, ListPRsInput
        mock_api = AsyncMock(spec=GitHubSkillAPI)
        mock_api.list_prs.return_value = [
            {"number": 1, "title": "PR1", "user": "a", "created_at": "t", "html_url": "u"}
        ]
        action = ListPRsAction(api=mock_api)
        result = await action.execute(ListPRsInput(repo="o/r"))
        assert result.success
        assert len(result.prs) == 1

    @pytest.mark.asyncio
    async def test_get_pr_checks_action(self):
        from skills.github.actions import GetPRChecksAction, GetPRChecksInput
        mock_api = AsyncMock(spec=GitHubSkillAPI)
        mock_api.get_pr_checks.return_value = {
            "pr_number": 1, "overall": "success",
            "check_runs": [{"name": "CI", "status": "completed", "conclusion": "success"}],
        }
        action = GetPRChecksAction(api=mock_api)
        result = await action.execute(GetPRChecksInput(repo="o/r", pr_number=1))
        assert result.overall == "success"

    @pytest.mark.asyncio
    async def test_ci_status_action(self):
        from skills.github.actions import CIStatusAction, CIStatusInput
        mock_api = AsyncMock(spec=GitHubSkillAPI)
        mock_api.list_wf_runs.return_value = [
            {"name": "Build", "status": "completed", "conclusion": "success",
             "html_url": "u", "created_at": "t"}
        ]
        action = CIStatusAction(api=mock_api)
        result = await action.execute(CIStatusInput(repo="o/r"))
        assert result.success
        assert len(result.workflows) == 1


# ── Backward-compatible GitHubClient shim ────────────────────────────────────


class TestGitHubClientShim:
    def test_shim_delegates_to_api(self):
        from core.skills.github_client import GitHubClient
        client = GitHubClient(token="fake-token")
        assert hasattr(client, "_api")
        assert isinstance(client._api, GitHubSkillAPI)

    def test_rate_limit_constant_exported(self):
        from core.skills.github_client import RATE_LIMIT_THRESHOLD
        assert RATE_LIMIT_THRESHOLD == 10
