"""Real GitHub API tests using VCR.py for recording/replaying.

These tests use real GitHub API calls on first run, then replay from cassettes.
To re-record: delete fixtures/vcr_cassettes/*.yaml and run tests.

Set GITHUB_TOKEN environment variable for real API calls.
"""

import os
from pathlib import Path

import pytest
import vcr

from core.skills.github_client import GitHubClient


# VCR configuration
def normalize_github_url(request):
    """Normalize GitHub URLs to use api.github.com."""
    # Replace enterprise GitHub URL with public GitHub
    request.uri = request.uri.replace("10.222.1.111:8080", "api.github.com")
    request.uri = request.uri.replace("http://", "https://")
    return request


vcr_config = vcr.VCR(
    cassette_library_dir=str(Path(__file__).parent.parent.parent / "fixtures" / "vcr_cassettes"),
    record_mode="once",  # Record once, then replay
    match_on=["uri", "method"],
    filter_headers=["authorization"],  # Don't record tokens
    decode_compressed_response=True,
    before_record_request=normalize_github_url,
)


@pytest.fixture
def github_client(db_session):
    """GitHub client with real token (if available)."""
    token = os.getenv("GITHUB_TOKEN")
    client = GitHubClient(db_session, token=token)
    yield client


@pytest.mark.skipif(
    not os.getenv("GITHUB_TOKEN") and not Path("fixtures/vcr_cassettes").exists(),
    reason="No GitHub token and no cassettes available",
)
@pytest.mark.asyncio
@vcr_config.use_cassette("github_get_pr.yaml")
async def test_real_github_get_pr(github_client):
    """Test getting a real PR from GitHub (octocat/Hello-World)."""
    # Use GitHub's official test repository
    # First, we need to add it to our database
    from core.repos import AccessScope, OwnerType, RepoRegistry, RepoType
    from api.models import Repo

    registry = RepoRegistry(lambda: github_client._session)

    # Check if repo exists
    existing = github_client._session.query(Repo).filter(
        Repo.repo_url == "https://github.com/octocat/Hello-World"
    ).first()

    if existing:
        repo_id = existing.repo_id
    else:
        repo = registry.create(
            repo_url="https://github.com/octocat/Hello-World",
            repo_type=RepoType.CODE,
            owner_id="test_user",
            owner_type=OwnerType.USER,
            access_scope=AccessScope.READ,
        )
        repo_id = repo.repo_id

    # Get a real PR (PR #1 exists in Hello-World)
    pr = await github_client.get_pr(repo_id, pr_number=1)

    assert pr["number"] == 1
    assert "title" in pr
    assert "body" in pr
    assert pr["state"] in ["open", "closed"]
    assert pr["changed_files"] >= 0


@pytest.mark.skipif(
    not os.getenv("GITHUB_TOKEN") and not Path("fixtures/vcr_cassettes").exists(),
    reason="No GitHub token and no cassettes available",
)
@pytest.mark.asyncio
@vcr_config.use_cassette("github_list_prs.yaml")
async def test_real_github_list_prs(github_client):
    """Test listing real PRs from GitHub."""
    from core.repos import AccessScope, OwnerType, RepoRegistry, RepoType
    from api.models import Repo

    registry = RepoRegistry(lambda: github_client._session)

    existing = github_client._session.query(Repo).filter(
        Repo.repo_url == "https://github.com/octocat/Hello-World"
    ).first()

    if existing:
        repo_id = existing.repo_id
    else:
        repo = registry.create(
            repo_url="https://github.com/octocat/Hello-World",
            repo_type=RepoType.CODE,
            owner_id="test_user",
            owner_type=OwnerType.USER,
            access_scope=AccessScope.READ,
        )
        repo_id = repo.repo_id

    prs = await github_client.list_prs(repo_id, state="all", limit=5)

    assert isinstance(prs, list)
    assert len(prs) > 0

    for pr in prs:
        assert "number" in pr
        assert "title" in pr
        assert "state" in pr


@pytest.mark.skipif(
    not os.getenv("GITHUB_TOKEN") and not Path("fixtures/vcr_cassettes").exists(),
    reason="No GitHub token and no cassettes available",
)
@pytest.mark.asyncio
@vcr_config.use_cassette("github_rate_limit.yaml")
async def test_real_github_rate_limit(github_client):
    """Test getting rate limit from GitHub."""
    rate_limit = github_client.get_rate_limit()

    assert "limit" in rate_limit
    assert "remaining" in rate_limit
    # Allow 0 for unauthenticated or when cassette doesn't have rate limit
    assert rate_limit["limit"] >= 0
