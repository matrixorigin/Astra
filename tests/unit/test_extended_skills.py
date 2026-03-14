"""Tests for extended skills — Issue 1 (undefined var), Issue 2 (async mismatch), Issue 3 (rate limit)."""

import inspect
import time
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from core.skills.extended import (
    AnalyzeBugSkill,
    CodeReviewInput,
    CodeReviewSkill,
    GenerateTestsSkill,
    SearchCodeSkill,
    register_extended_skills,
)


# ---------------------------------------------------------------------------
# Issue 1: __init__ accepts github and db_factory without error
# ---------------------------------------------------------------------------


class TestInitParams:
    def test_code_review_accepts_db_factory(self):
        gh = MagicMock()
        factory = MagicMock()
        skill = CodeReviewSkill(github=gh, db_factory=factory)
        assert skill.github is gh

    def test_search_code_accepts_db_factory(self):
        gh = MagicMock()
        factory = MagicMock()
        skill = SearchCodeSkill(github=gh, db_factory=factory)
        assert skill.github is gh

    def test_generate_tests_default_db_factory_is_none(self):
        gh = MagicMock()
        # Should not raise — db_factory is optional
        skill = GenerateTestsSkill(github=gh)
        assert skill.github is gh

    def test_analyze_bug_accepts_db_factory(self):
        gh = MagicMock()
        skill = AnalyzeBugSkill(github=gh, db_factory=MagicMock())
        assert skill.github is gh


# ---------------------------------------------------------------------------
# Issue 2: execute() is async (matches base class)
# ---------------------------------------------------------------------------


class TestAsyncExecute:
    def test_code_review_execute_is_coroutine(self):
        assert inspect.iscoroutinefunction(CodeReviewSkill.execute)

    def test_search_code_execute_is_coroutine(self):
        assert inspect.iscoroutinefunction(SearchCodeSkill.execute)

    def test_generate_tests_execute_is_coroutine(self):
        assert inspect.iscoroutinefunction(GenerateTestsSkill.execute)

    def test_analyze_bug_execute_is_coroutine(self):
        assert inspect.iscoroutinefunction(AnalyzeBugSkill.execute)

    @pytest.mark.asyncio
    async def test_code_review_awaitable(self):
        gh = MagicMock()
        gh.get_pr = AsyncMock(
            return_value={
                "changed_files": 3,
                "additions": 10,
                "deletions": 5,
            }
        )
        skill = CodeReviewSkill(github=gh)
        inp = CodeReviewInput(repo_id=1, pr_number=42)
        out = await skill.execute(inp)
        assert out.success is True
        assert out.review["pr_number"] == 42
        gh.get_pr.assert_awaited_once_with(1, 42)


# ---------------------------------------------------------------------------
# Issue 1 (bonus): register_extended_skills passes args in correct order
# ---------------------------------------------------------------------------


class TestRegistration:
    def test_register_passes_github_first(self):
        registry = MagicMock()
        db = MagicMock()
        gh = MagicMock()
        register_extended_skills(registry, db, github=gh)
        # All 4 skills registered
        assert registry.register.call_count == 4
        for call in registry.register.call_args_list:
            skill = call.kwargs["skill"]
            assert skill.github is gh


# ---------------------------------------------------------------------------
# Issue 3: GitHubClient._check_rate_limit
# ---------------------------------------------------------------------------


class TestRateLimit:
    @pytest.mark.asyncio
    async def test_check_rate_limit_waits_when_low(self):
        from core.skills.github_client import GitHubClient, RATE_LIMIT_THRESHOLD

        client_mock = MagicMock()
        core_mock = MagicMock()
        core_mock.remaining = RATE_LIMIT_THRESHOLD - 1
        core_mock.limit = 5000
        # reset 2 seconds from now
        core_mock.reset = MagicMock()
        core_mock.reset.timestamp.return_value = time.time() + 2
        rl_mock = MagicMock()
        rl_mock.core = core_mock
        client_mock.get_rate_limit.return_value = rl_mock

        gh = object.__new__(GitHubClient)
        gh.client = client_mock
        gh._rl_checked_at = 0  # force stale

        with patch("core.skills.github_client.asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
            await gh._check_rate_limit()
            mock_sleep.assert_awaited_once()
            # should wait ~3s (2 + 1 buffer), capped at 60
            wait_arg = mock_sleep.call_args[0][0]
            assert 1 < wait_arg <= 60

    @pytest.mark.asyncio
    async def test_check_rate_limit_skips_when_plenty(self):
        from core.skills.github_client import GitHubClient, RATE_LIMIT_THRESHOLD

        client_mock = MagicMock()
        core_mock = MagicMock()
        core_mock.remaining = RATE_LIMIT_THRESHOLD + 100
        rl_mock = MagicMock()
        rl_mock.core = core_mock
        client_mock.get_rate_limit.return_value = rl_mock

        gh = object.__new__(GitHubClient)
        gh.client = client_mock
        gh._rl_checked_at = 0  # force stale

        with patch("core.skills.github_client.asyncio.sleep", new_callable=AsyncMock) as mock_sleep:
            await gh._check_rate_limit()
            mock_sleep.assert_not_awaited()

    @pytest.mark.asyncio
    async def test_check_rate_limit_tolerates_failure(self):
        """Rate limit check failure should not propagate."""
        from core.skills.github_client import GitHubClient

        client_mock = MagicMock()
        client_mock.get_rate_limit.side_effect = RuntimeError("network")

        gh = object.__new__(GitHubClient)
        gh.client = client_mock
        gh._rl_checked_at = 0

        # Should not raise
        await gh._check_rate_limit()

    @pytest.mark.asyncio
    async def test_check_rate_limit_cached_within_60s(self):
        """Should skip API call if checked recently."""
        from core.skills.github_client import GitHubClient

        client_mock = MagicMock()
        gh = object.__new__(GitHubClient)
        gh.client = client_mock
        gh._rl_checked_at = time.monotonic()  # just checked

        await gh._check_rate_limit()
        client_mock.get_rate_limit.assert_not_called()
