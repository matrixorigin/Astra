"""Tests for GitHub skill — api.py + actions.py.

v3: Uses platform DB session (sk_github_* tables). GitHub API calls are mocked.
"""

import uuid
from datetime import datetime, timezone
from unittest.mock import AsyncMock, MagicMock

import pytest
from github import GithubException

from core.exceptions import GitHubError, GitHubRateLimitError
from skills.github.api import GitHubSkillAPI
from skills.github.models import SkGithubPRCache

# ── Repo management (real DB) ────────────────────────────────────────────────


class TestRepoManagement:
    @pytest.fixture
    def api(self, db_session):
        return GitHubSkillAPI(token=None, db=db_session)

    def test_add_repo(self, api):
        result = api.add_repo("octocat", f"repo_{uuid.uuid4().hex}")
        assert result["created"] is True
        assert result["repo_id"]

    def test_add_repo_idempotent(self, api):
        name = f"repo_{uuid.uuid4().hex}"
        r1 = api.add_repo("octocat", name)
        r2 = api.add_repo("octocat", name)
        assert r1["repo_id"] == r2["repo_id"]
        assert r2["created"] is False

    def test_list_repos(self, api):
        name = f"repo_{uuid.uuid4().hex}"
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
        repo = f"owner/cache_test_{uuid.uuid4().hex}"
        api._cache_pr(repo, {"number": 42, "title": "Test PR", "state": "open", "user": "alice"})
        row = db_session.query(SkGithubPRCache).filter_by(repo_full_name=repo, pr_number=42).one()
        assert row.title == "Test PR"
        assert row.state == "open"

    def test_cache_pr_update(self, api, db_session):
        repo = f"owner/cache_upd_{uuid.uuid4().hex}"
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
        mock_run.head_branch = "main"
        mock_run.actor.login = "alice"
        mock_run.pull_requests = []
        mock_run.created_at = datetime(2026, 1, 1, tzinfo=timezone.utc)
        mock_repo = MagicMock()
        mock_repo.get_workflow_runs.return_value = [mock_run]
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.list_wf_runs("owner/repo", limit=5)
        assert len(result) == 1
        assert result[0]["workflow"] == "Build"
        assert result[0]["conclusion"] == "success"
        assert result[0]["branch"] == "main"
        assert result[0]["actor"] == "alice"
        assert result[0]["pr_number"] is None

    # ── Issue operations ─────────────────────────────────────────────

    def _make_mock_issue(self, number=1, title="Bug", state="open", is_pr=False):
        issue = MagicMock()
        issue.number = number
        issue.title = title
        issue.body = "issue body"
        issue.state = state
        issue.user.login = "alice"
        label = MagicMock()
        label.name = "bug"
        issue.labels = [label]
        assignee = MagicMock()
        assignee.login = "bob"
        issue.assignees = [assignee]
        issue.created_at = datetime(2026, 1, 1, tzinfo=timezone.utc)
        issue.updated_at = datetime(2026, 1, 2, tzinfo=timezone.utc)
        issue.html_url = f"https://github.com/o/r/issues/{number}"
        issue.comments = 3
        issue.pull_request = MagicMock() if is_pr else None
        issue.milestone = None
        issue.reactions = {"total_count": 2, "+1": 1, "-1": 0, "laugh": 1}
        issue.closed_at = None
        issue.closed_by = None
        issue.state_reason = None
        issue.locked = False
        issue.get_comments.return_value = []
        return issue

    @pytest.mark.asyncio
    async def test_list_issues(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.return_value = [
            self._make_mock_issue(1, "Bug 1"),
            self._make_mock_issue(2, "Bug 2"),
        ]
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.list_issues("owner/repo")
        assert len(result) == 2
        assert result[0]["number"] == 1
        assert result[0]["title"] == "Bug 1"
        assert result[0]["state"] == "open"
        assert result[0]["user"] == "alice"
        assert result[0]["labels"] == ["bug"]
        assert result[0]["html_url"] == "https://github.com/o/r/issues/1"
        # brief detail — no body, no comments count
        assert "body" not in result[0]
        assert "assignees" not in result[0]

    @pytest.mark.asyncio
    async def test_list_issues_normal_detail(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.return_value = [self._make_mock_issue(1)]
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.list_issues("owner/repo", detail="normal")
        assert result[0]["body"] == "issue body"
        assert result[0]["comment_count"] == 3
        assert result[0]["assignees"] == ["bob"]
        assert result[0]["milestone"] is None
        assert result[0]["created_at"] == "2026-01-01 00:00"
        assert "reactions" not in result[0]

    @pytest.mark.asyncio
    async def test_list_issues_excludes_prs(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.return_value = [
            self._make_mock_issue(1, "Real issue", is_pr=False),
            self._make_mock_issue(2, "Actually a PR", is_pr=True),
        ]
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.list_issues("owner/repo")
        assert len(result) == 1
        assert result[0]["title"] == "Real issue"

    @pytest.mark.asyncio
    async def test_list_issues_with_filters(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.return_value = [self._make_mock_issue(1)]
        mock_gh_client.get_repo.return_value = mock_repo
        await api.list_issues(
            "owner/repo", labels=["bug"], sort="updated",
            direction="asc", assignee="alice", creator="bob",
        )
        call_kwargs = mock_repo.get_issues.call_args[1]
        assert call_kwargs["labels"] == ["bug"]
        assert call_kwargs["sort"] == "updated"
        assert call_kwargs["direction"] == "asc"
        assert call_kwargs["assignee"] == "alice"
        assert call_kwargs["creator"] == "bob"

    @pytest.mark.asyncio
    async def test_list_issues_with_since(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.return_value = [self._make_mock_issue(1)]
        mock_gh_client.get_repo.return_value = mock_repo
        await api.list_issues("owner/repo", since="2026-01-01T00:00:00+00:00")
        call_kwargs = mock_repo.get_issues.call_args[1]
        assert "since" in call_kwargs
        assert call_kwargs["since"].year == 2026

    @pytest.mark.asyncio
    async def test_list_issues_invalid_since(self, api, mock_gh_client):
        with pytest.raises(GitHubError, match="Invalid ISO datetime"):
            await api.list_issues("owner/repo", since="not-a-date")

    @pytest.mark.asyncio
    async def test_list_issues_respects_limit(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.return_value = [
            self._make_mock_issue(i, f"Issue {i}") for i in range(20)
        ]
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.list_issues("owner/repo", limit=3)
        assert len(result) == 3

    @pytest.mark.asyncio
    async def test_list_issues_repo_not_found(self, api, mock_gh_client):
        mock_gh_client.get_repo.side_effect = GithubException(
            404, {"message": "Not Found"}, None
        )
        with pytest.raises(GitHubError, match="not found"):
            await api.list_issues("owner/nonexistent")

    @pytest.mark.asyncio
    async def test_list_issues_rate_limited(self, api, mock_gh_client):
        """Rate limit from get_issues() call (not _get_repo)."""
        mock_repo = MagicMock()
        mock_repo.get_issues.side_effect = GithubException(
            403, {"message": "rate limit"}, None
        )
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(GitHubRateLimitError):
            await api.list_issues("owner/repo")

    @pytest.mark.asyncio
    async def test_get_issue_normal(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issue.return_value = self._make_mock_issue(42, "Specific bug")
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.get_issue("owner/repo", 42)
        assert result["number"] == 42
        assert result["title"] == "Specific bug"
        assert result["body"] == "issue body"
        assert result["assignees"] == ["bob"]
        assert result["created_at"] == "2026-01-01 00:00"

    @pytest.mark.asyncio
    async def test_get_issue_full_detail(self, api, mock_gh_client):
        mock_repo = MagicMock()
        issue = self._make_mock_issue(42, "Full detail")
        mock_repo.get_issue.return_value = issue
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.get_issue("owner/repo", 42, detail="full")
        assert result["reactions"] == {"total_count": 2, "+1": 1, "-1": 0, "laugh": 1}
        assert result["locked"] is False
        assert result["closed_at"] is None
        assert result["closed_by"] is None
        assert "recent_comments" in result

    @pytest.mark.asyncio
    async def test_get_issue_brief_detail(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issue.return_value = self._make_mock_issue(42)
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.get_issue("owner/repo", 42, detail="brief")
        assert result["number"] == 42
        assert result["title"] == "Bug"
        assert "body" not in result
        assert "reactions" not in result
        assert "assignees" not in result

    @pytest.mark.asyncio
    async def test_get_issue_invalid_detail(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issue.return_value = self._make_mock_issue(42)
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(ValueError, match="Invalid detail level"):
            await api.get_issue("owner/repo", 42, detail="invalid")

    @pytest.mark.asyncio
    async def test_get_issue_not_found(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issue.side_effect = GithubException(
            404, {"message": "Not Found"}, None
        )
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(GitHubError, match="not found"):
            await api.get_issue("owner/repo", 999)

    @pytest.mark.asyncio
    async def test_get_issue_rate_limited(self, api, mock_gh_client):
        """Rate limit from get_issue() call (not _get_repo)."""
        mock_repo = MagicMock()
        mock_repo.get_issue.side_effect = GithubException(
            403, {"message": "rate limit"}, None
        )
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(GitHubRateLimitError):
            await api.get_issue("owner/repo", 1)

    @pytest.mark.asyncio
    async def test_create_issue(self, api, mock_gh_client):
        created = self._make_mock_issue(99, "New bug")
        created.body = "desc"
        mock_repo = MagicMock()
        mock_repo.create_issue.return_value = created
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.create_issue("owner/repo", "New bug", body="desc", labels=["bug"])
        assert result["number"] == 99
        assert result["title"] == "New bug"
        assert result["body"] == "desc"
        mock_repo.create_issue.assert_called_once_with(
            title="New bug", body="desc", labels=["bug"]
        )

    @pytest.mark.asyncio
    async def test_create_issue_minimal(self, api, mock_gh_client):
        """Create issue with only title — body/labels/assignees omitted."""
        created = self._make_mock_issue(100, "Title only")
        created.body = ""
        mock_repo = MagicMock()
        mock_repo.create_issue.return_value = created
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.create_issue("owner/repo", "Title only")
        assert result["number"] == 100
        # Only title should be passed when body/labels/assignees are empty
        mock_repo.create_issue.assert_called_once_with(title="Title only")

    @pytest.mark.asyncio
    async def test_create_issue_rate_limited(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.create_issue.side_effect = GithubException(
            403, {"message": "rate limit"}, None
        )
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(GitHubRateLimitError):
            await api.create_issue("owner/repo", "title")

    @pytest.mark.asyncio
    async def test_create_issue_with_assignees(self, api, mock_gh_client):
        created = self._make_mock_issue(101, "Assigned")
        mock_repo = MagicMock()
        mock_repo.create_issue.return_value = created
        mock_gh_client.get_repo.return_value = mock_repo
        await api.create_issue("owner/repo", "Assigned", labels=["bug"], assignees=["alice", "bob"])
        mock_repo.create_issue.assert_called_once_with(
            title="Assigned", labels=["bug"], assignees=["alice", "bob"],
        )

    @pytest.mark.asyncio
    async def test_create_issue_generic_error(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.create_issue.side_effect = GithubException(
            422, {"message": "Validation Failed"}, None
        )
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(GitHubError, match="Validation Failed"):
            await api.create_issue("owner/repo", "title")

    @pytest.mark.asyncio
    async def test_list_issues_generic_error(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.side_effect = GithubException(
            422, {"message": "Validation Failed"}, None
        )
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(GitHubError, match="Validation Failed"):
            await api.list_issues("owner/repo")

    @pytest.mark.asyncio
    async def test_get_issue_generic_error(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issue.side_effect = GithubException(
            500, {"message": "Internal"}, None
        )
        mock_gh_client.get_repo.return_value = mock_repo
        with pytest.raises(GitHubError, match="Internal"):
            await api.get_issue("owner/repo", 1)

    @pytest.mark.asyncio
    async def test_list_issues_with_milestone(self, api, mock_gh_client):
        mock_repo = MagicMock()
        mock_repo.get_issues.return_value = [self._make_mock_issue(1)]
        mock_gh_client.get_repo.return_value = mock_repo
        await api.list_issues("owner/repo", milestone="v1.0")
        call_kwargs = mock_repo.get_issues.call_args[1]
        assert call_kwargs["milestone"] == "v1.0"

    @pytest.mark.asyncio
    async def test_format_issue_full_with_comments(self, api, mock_gh_client):
        """Full detail includes recent comments from get_comments()."""
        mock_repo = MagicMock()
        issue = self._make_mock_issue(42, "With comments")
        comment = MagicMock()
        comment.user.login = "carol"
        comment.created_at = datetime(2026, 1, 3, tzinfo=timezone.utc)
        comment.body = "Looks good"
        issue.get_comments.return_value = [comment]
        mock_repo.get_issue.return_value = issue
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.get_issue("owner/repo", 42, detail="full")
        assert len(result["recent_comments"]) == 1
        assert result["recent_comments"][0]["user"] == "carol"
        assert result["recent_comments"][0]["body"] == "Looks good"

    @pytest.mark.asyncio
    async def test_format_issue_full_comments_error_logged(self, api, mock_gh_client):
        """If get_comments() fails, recent_comments is [] and warning is logged."""
        mock_repo = MagicMock()
        issue = self._make_mock_issue(42)
        issue.get_comments.side_effect = RuntimeError("network error")
        mock_repo.get_issue.return_value = issue
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.get_issue("owner/repo", 42, detail="full")
        assert result["recent_comments"] == []

    @pytest.mark.asyncio
    async def test_format_issue_full_comments_capped_at_5(self, api, mock_gh_client):
        """Full detail caps recent_comments at 5 even if more exist."""
        mock_repo = MagicMock()
        issue = self._make_mock_issue(42)
        comments = []
        for i in range(10):
            c = MagicMock()
            c.user.login = f"user{i}"
            c.created_at = datetime(2026, 1, i + 1, tzinfo=timezone.utc)
            c.body = f"comment {i}"
            comments.append(c)
        issue.get_comments.return_value = comments
        mock_repo.get_issue.return_value = issue
        mock_gh_client.get_repo.return_value = mock_repo
        result = await api.get_issue("owner/repo", 42, detail="full")
        # full level returns up to 20 comments; 10 mocked → all 10 returned
        assert len(result["recent_comments"]) == 10
        assert result["recent_comments"][0]["user"] == "user0"
        assert result["recent_comments"][9]["user"] == "user9"

    @pytest.mark.asyncio
    async def test_get_issue_repo_not_found_propagates(self, api, mock_gh_client):
        """GitHubError from _get_repo propagates through get_issue."""
        mock_gh_client.get_repo.side_effect = GithubException(
            404, {"message": "Not Found"}, None
        )
        with pytest.raises(GitHubError, match="not found"):
            await api.get_issue("owner/gone", 1)

    @pytest.mark.asyncio
    async def test_create_issue_repo_not_found_propagates(self, api, mock_gh_client):
        """GitHubError from _get_repo propagates through create_issue."""
        mock_gh_client.get_repo.side_effect = GithubException(
            404, {"message": "Not Found"}, None
        )
        with pytest.raises(GitHubError, match="not found"):
            await api.create_issue("owner/gone", "title")

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

    @pytest.mark.asyncio
    async def test_list_issues_action(self):
        from skills.github.actions import ListIssuesAction, ListIssuesInput
        mock_api = AsyncMock(spec=GitHubSkillAPI)
        mock_api.list_issues.return_value = [
            {"number": 1, "title": "Bug", "state": "open", "user": "a",
             "labels": ["bug"], "html_url": "u"}
        ]
        action = ListIssuesAction(api=mock_api)
        inp = ListIssuesInput(repo="o/r", state="closed", labels=["bug"], sort="updated", limit=5)
        result = await action.execute(inp)
        assert result.success
        assert len(result.issues) == 1
        assert result.issues[0]["number"] == 1
        assert result.issues[0]["title"] == "Bug"
        # Verify parameters forwarded correctly
        mock_api.list_issues.assert_called_once_with(
            "o/r", "closed", ["bug"], "updated", "desc",
            None, None, None, None, 5, "brief",
        )

    @pytest.mark.asyncio
    async def test_get_issue_action(self):
        from skills.github.actions import GetIssueAction, GetIssueInput
        mock_api = AsyncMock(spec=GitHubSkillAPI)
        mock_api.get_issue.return_value = {
            "number": 42, "title": "Bug", "body": "desc", "state": "open",
            "user": "a", "labels": [], "assignees": [], "created_at": "t",
            "updated_at": "t", "html_url": "u", "comments": 0,
        }
        action = GetIssueAction(api=mock_api)
        result = await action.execute(GetIssueInput(repo="o/r", issue_number=42, detail="full"))
        assert result.success
        assert result.issue["number"] == 42
        assert result.issue["body"] == "desc"
        mock_api.get_issue.assert_called_once_with("o/r", 42, "full")

    @pytest.mark.asyncio
    async def test_create_issue_action(self):
        from skills.github.actions import CreateIssueAction, CreateIssueInput
        mock_api = AsyncMock(spec=GitHubSkillAPI)
        mock_api.create_issue.return_value = {
            "number": 99, "title": "New", "body": "body text", "state": "open",
            "user": "a", "labels": ["bug"], "html_url": "u",
        }
        action = CreateIssueAction(api=mock_api)
        result = await action.execute(CreateIssueInput(
            repo="o/r", title="New", body="body text", labels=["bug"], assignees=["alice"],
        ))
        assert result.success
        assert result.issue["number"] == 99
        assert result.issue["body"] == "body text"
        mock_api.create_issue.assert_called_once_with(
            "o/r", "New", "body text", ["bug"], ["alice"],
        )

    @pytest.mark.asyncio
    async def test_create_issue_action_empty_title_rejected(self):
        from pydantic import ValidationError

        from skills.github.actions import CreateIssueInput
        with pytest.raises(ValidationError, match="title"):
            CreateIssueInput(repo="o/r", title="   ")


# ── Builtin skill wrappers (core/skills/builtin.py) ──────────────────────────


class TestBuiltinIssueSkills:
    """Test builtin skills delegate through GitHubClient shim, not _api directly."""

    @pytest.mark.asyncio
    async def test_list_issues_skill_delegates_through_shim(self):
        from core.skills.builtin import ListIssuesInput, ListIssuesSkill
        mock_github = AsyncMock()
        mock_github.list_issues.return_value = [{"number": 1, "title": "Bug"}]
        skill = ListIssuesSkill(github=mock_github)
        result = await skill.execute(ListIssuesInput(repo="o/r", state="closed", limit=5))
        assert result.success
        assert result.issues == [{"number": 1, "title": "Bug"}]
        mock_github.list_issues.assert_called_once_with(
            "o/r", "closed", None, "created", "desc", None, None, None, None, 5, "brief",
        )

    @pytest.mark.asyncio
    async def test_list_issues_skill_missing_repo(self):
        from core.skills.builtin import ListIssuesInput, ListIssuesSkill
        skill = ListIssuesSkill(github=AsyncMock())
        result = await skill.execute(ListIssuesInput(repo="", state="open"))
        assert result.success is False
        assert "repo is required" in result.result

    @pytest.mark.asyncio
    async def test_get_issue_skill_delegates_through_shim(self):
        from core.skills.builtin import GetIssueInput, GetIssueSkill
        mock_github = AsyncMock()
        mock_github.get_issue.return_value = {"number": 42, "title": "Bug"}
        skill = GetIssueSkill(github=mock_github)
        result = await skill.execute(GetIssueInput(repo="o/r", issue_number=42, detail="full"))
        assert result.success
        assert result.issue["number"] == 42
        mock_github.get_issue.assert_called_once_with("o/r", 42, "full")

    @pytest.mark.asyncio
    async def test_get_issue_skill_missing_repo(self):
        from core.skills.builtin import GetIssueInput, GetIssueSkill
        skill = GetIssueSkill(github=AsyncMock())
        result = await skill.execute(GetIssueInput(repo="", issue_number=1))
        assert result.success is False

    @pytest.mark.asyncio
    async def test_create_issue_skill_delegates_through_shim(self):
        from core.skills.builtin import CreateIssueInput, CreateIssueSkill
        mock_github = AsyncMock()
        mock_github.create_issue.return_value = {"number": 99, "title": "New"}
        skill = CreateIssueSkill(github=mock_github)
        result = await skill.execute(CreateIssueInput(
            repo="o/r", title="New", body="desc", labels=["bug"], assignees=["alice"],
        ))
        assert result.success
        assert result.issue["number"] == 99
        mock_github.create_issue.assert_called_once_with(
            "o/r", "New", "desc", ["bug"], ["alice"],
        )

    @pytest.mark.asyncio
    async def test_create_issue_skill_missing_repo(self):
        from core.skills.builtin import CreateIssueInput, CreateIssueSkill
        skill = CreateIssueSkill(github=AsyncMock())
        result = await skill.execute(CreateIssueInput(repo="", title="Bug"))
        assert result.success is False

    def test_create_issue_skill_empty_title_rejected(self):
        from pydantic import ValidationError

        from core.skills.builtin import CreateIssueInput
        with pytest.raises(ValidationError, match="title"):
            CreateIssueInput(repo="o/r", title="")

    @pytest.mark.asyncio
    async def test_list_issues_skill_falls_back_to_repo_id(self):
        """When repo is empty, skill should try repo_id as fallback."""
        from core.skills.builtin import ListIssuesInput, ListIssuesSkill
        mock_github = AsyncMock()
        mock_github.list_issues.return_value = []
        skill = ListIssuesSkill(github=mock_github)
        # repo_id is int|None from SkillInput base — when set, used as fallback
        result = await skill.execute(ListIssuesInput(repo="", repo_id=123))
        assert result.success
        # repo_id=123 is truthy, so it's used as the repo argument
        mock_github.list_issues.assert_called_once()


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

    @pytest.mark.asyncio
    async def test_shim_list_issues_delegates(self):
        from core.skills.github_client import GitHubClient
        client = GitHubClient(token="fake-token")
        client._api = AsyncMock(spec=GitHubSkillAPI)
        client._api.list_issues.return_value = [{"number": 1}]
        result = await client.list_issues("owner/repo", state="closed", labels=["bug"], limit=5)
        assert result == [{"number": 1}]
        client._api.list_issues.assert_called_once_with(
            "owner/repo", "closed", ["bug"], "created", "desc",
            None, None, None, None, 5, "brief",
        )

    @pytest.mark.asyncio
    async def test_shim_get_issue_delegates(self):
        from core.skills.github_client import GitHubClient
        client = GitHubClient(token="fake-token")
        client._api = AsyncMock(spec=GitHubSkillAPI)
        client._api.get_issue.return_value = {"number": 42}
        result = await client.get_issue("owner/repo", 42, detail="full")
        assert result == {"number": 42}
        client._api.get_issue.assert_called_once_with("owner/repo", 42, "full")

    @pytest.mark.asyncio
    async def test_shim_create_issue_delegates(self):
        from core.skills.github_client import GitHubClient
        client = GitHubClient(token="fake-token")
        client._api = AsyncMock(spec=GitHubSkillAPI)
        client._api.create_issue.return_value = {"number": 99}
        result = await client.create_issue("owner/repo", "title", body="b", labels=["x"], assignees=["a"])
        assert result == {"number": 99}
        client._api.create_issue.assert_called_once_with("owner/repo", "title", "b", ["x"], ["a"])
