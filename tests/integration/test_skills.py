"""Tests for skill framework."""

import pytest

from core.llm import LLMClient
from core.skills import (
    SkillRegistry,
)
from core.skills.builtin import CIStatusSkill, ListPRsSkill, SummarizePRSkill
from core.skills.github_client import GitHubClient


@pytest.fixture
def registry(db_session):
    """Skill registry fixture"""
    return SkillRegistry(lambda: db_session)


@pytest.fixture
def github(db_session):
    """GitHub client fixture"""
    return GitHubClient(db_session)


@pytest.fixture
def llm(db_session):
    """LLM client fixture"""
    return LLMClient(lambda: db_session)


def test_skill_registry_register(db_session, registry, llm, github):
    """Test skill registration"""
    # Clean up first
    from api.models import SkillRegistry as SkillModel
    db_session.query(SkillModel).filter(SkillModel.skill_name == "summarize_pr").delete()
    db_session.commit()
    
    skill = SummarizePRSkill(llm, github, db_session)
    registry.register(skill)

    # Check in-memory
    assert registry.get("summarize_pr") is not None
    assert registry.get("summarize_pr").version == "1.0.0"

    # Check database using ORM
    row = db_session.query(SkillModel).filter(SkillModel.skill_name == "summarize_pr").first()
    assert row is not None
    assert row.version == "1.0.0"
    assert row.is_active in (1, True)


def test_skill_registry_versioning(db_session, registry, llm, github):
    """Test skill versioning"""
    # Clean up first
    from api.models import SkillRegistry as SkillModel
    db_session.query(SkillModel).filter(SkillModel.skill_name == "summarize_pr").delete()
    db_session.commit()
    
    # Register v1.0.0
    skill_v1 = SummarizePRSkill(llm, github, db_session)
    skill_v1.version = "1.0.0"
    registry.register(skill_v1)

    # Register v1.1.0 (should deactivate v1.0.0)
    skill_v2 = SummarizePRSkill(llm, github, db_session)
    skill_v2.version = "1.1.0"
    registry.register(skill_v2)

    # Check active version
    assert registry.get("summarize_pr").version == "1.1.0"

    # Check specific version
    assert registry.get("summarize_pr", version="1.0.0") is not None
    assert registry.get("summarize_pr", version="1.1.0") is not None

    # Check database using ORM
    from api.models import SkillRegistry as SkillModel
    rows = db_session.query(SkillModel).filter(
        SkillModel.skill_name == "summarize_pr"
    ).order_by(SkillModel.version).all()
    
    assert len(rows) == 2
    assert rows[0].version == "1.0.0"
    assert rows[0].is_active in (0, False)
    assert rows[1].version == "1.1.0"
    assert rows[1].is_active in (1, True)


def test_skill_registry_list_available(db_session, registry, llm, github):
    """Test listing available skills for a repo"""
    # Register skills
    registry.register(SummarizePRSkill(llm, github, db_session))
    registry.register(ListPRsSkill(github, db_session))
    registry.register(CIStatusSkill(github, db_session))

    # Create a CODE repo with unique URL
    import time

    from core.repos import (
        AccessScope as AccessScopeEnum,
    )
    from core.repos import (
        OwnerType,
        RepoRegistry,
    )
    from core.repos import (
        RepoType as RepoTypeEnum,
    )

    repo_registry = RepoRegistry(lambda: db_session)
    repo = repo_registry.create(
        repo_url=f"https://github.com/test/repo-{int(time.time())}",
        repo_type=RepoTypeEnum.CODE,
        owner_id=f"test_user_{int(time.time())}",
        owner_type=OwnerType.USER,
        access_scope=AccessScopeEnum.READ,
    )

    # List available skills
    available = registry.list_available(repo.repo_id)

    # Should have summarize_pr and list_prs (both need CODE repo)
    # ci_status needs CI or CODE, so it should also be available
    assert len(available) == 3
    skill_names = [s.name for s in available]
    assert "summarize_pr" in skill_names
    assert "list_prs" in skill_names
    assert "ci_status" in skill_names


@pytest.mark.asyncio
async def test_summarize_pr_skill(db_session, llm, github, monkeypatch):
    """Test summarize_pr skill execution"""
    # Mock LLM response
    from core.llm.models import LLMProvider, LLMResponse

    def mock_chat(*args, **kwargs):
        return LLMResponse(
            content="This PR adds a new feature",
            model="gpt-4",
            provider=LLMProvider.OPENAI,
            tokens_prompt=100,
            tokens_completion=50,
            tokens_total=150,
            latency_ms=1000,
            cost_usd=0.002,
        )

    # Mock GitHub API calls
    async def mock_get_pr(repo_id, pr_number, detail="normal"):
        return {
            "number": pr_number,
            "title": f"PR #{pr_number}",
            "body": "This is a test PR",
            "state": "open",
            "changed_files": 5,
            "additions": 120,
            "deletions": 30,
            "author": "test_user",
            "created_at": "2026-02-10 00:00",
            "html_url": f"https://github.com/owner/repo/pull/{pr_number}",
        }

    async def mock_get_pr_diff(repo_id, pr_number):
        return "diff --git a/file.py b/file.py\n+added line\n-removed line"

    monkeypatch.setattr(llm, "chat", mock_chat)
    monkeypatch.setattr(github, "get_pr", mock_get_pr)
    monkeypatch.setattr(github, "get_pr_diff", mock_get_pr_diff)

    skill = SummarizePRSkill(llm, github, db_session)

    input_data = {
        "repo_id": 1,
        "user_id": "test_user",
        "session_id": "test_session",
        "pr_number": 123,
        "include_diff": True,
    }

    input_obj = skill.validate_input(input_data)
    output = await skill.execute(input_obj)

    assert output.success is True
    assert output.summary is not None
    assert output.files_changed == 5
    assert output.cost >= 0


@pytest.mark.asyncio
async def test_list_prs_skill(db_session, github, monkeypatch):
    """Test list_prs skill execution"""

    # Mock GitHub API call
    async def mock_list_prs(repo_id, state, limit, detail="brief"):
        return [
            {
                "number": i,
                "title": f"PR #{i}",
                "author": "user",
                "state": state,
                "created_at": "2026-02-10 00:00",
                "html_url": f"https://github.com/owner/repo/pull/{i}",
            }
            for i in range(1, limit + 1)
        ]

    monkeypatch.setattr(github, "list_prs", mock_list_prs)

    skill = ListPRsSkill(github, db_session)

    input_data = {
        "repo_id": 1,
        "user_id": "test_user",
        "session_id": "test_session",
        "state": "open",
        "limit": 5,
    }

    input_obj = skill.validate_input(input_data)
    output = await skill.execute(input_obj)

    assert output.success is True
    assert len(output.prs) == 5
    assert output.cost == 0  # No LLM call


@pytest.mark.asyncio
async def test_ci_status_skill(db_session, github, monkeypatch):
    """Test ci_status skill execution"""

    # Mock GitHub API call
    async def mock_list_wf_runs(repo_id, limit, detail="brief"):
        return [
            {
                "workflow": f"Workflow {i}",
                "status": "completed",
                "conclusion": "success",
                "branch": "main",
                "pr_number": None,
                "actor": "alice",
                "url": f"https://github.com/owner/repo/actions/runs/{i}",
                "created_at": "2026-02-10T00:00:00Z",
            }
            for i in range(1, limit + 1)
        ]

    monkeypatch.setattr(github, "list_wf_runs", mock_list_wf_runs)

    skill = CIStatusSkill(github, db_session)

    input_data = {
        "repo_id": 1,
        "user_id": "test_user",
        "session_id": "test_session",
        "limit": 3,
    }

    input_obj = skill.validate_input(input_data)
    output = await skill.execute(input_obj)

    assert output.success is True
    assert len(output.workflows) == 3
    assert output.cost == 0  # No LLM call


@pytest.mark.asyncio
async def test_register_builtin_skills_then_execute(db_session):
    """Skills created by register_builtin_skills must actually execute.

    Regression test: register_builtin_skills previously passed db_factory as
    the first positional arg, so self.github was a sessionmaker instead of
    GitHubClient — only discovered at runtime.
    """
    from core.skills.builtin import register_builtin_skills

    db_factory = lambda: db_session

    # Mock GitHub methods on the client that register_builtin_skills will create
    mock_prs = [{"number": 1, "title": "PR #1", "user": "u", "created_at": "2026-01-01", "html_url": "http://x"}]
    mock_runs = [{"name": "CI", "status": "completed", "conclusion": "success", "html_url": "http://x", "created_at": "2026-01-01"}]

    registry = SkillRegistry(db_factory)

    from unittest.mock import AsyncMock, MagicMock
    fake_gh = MagicMock()
    fake_gh.list_prs = AsyncMock(return_value=mock_prs)
    fake_gh.list_wf_runs = AsyncMock(return_value=mock_runs)

    register_builtin_skills(registry, db_factory, github=fake_gh)

    # Retrieve from registry and execute — this is the path /chat/turn uses
    list_prs_skill = registry.get("list_prs")
    inp = list_prs_skill.validate_input({"repo": "matrixorigin/matrixone", "state": "open", "limit": 1})
    out = await list_prs_skill.execute(inp)
    assert out.success is True
    assert len(out.prs) == 1

    ci_skill = registry.get("ci_status")
    inp2 = ci_skill.validate_input({"repo": "matrixorigin/matrixone", "limit": 1})
    out2 = await ci_skill.execute(inp2)
    assert out2.success is True
    assert len(out2.workflows) == 1


@pytest.mark.asyncio
async def test_summarize_pr_works_with_sync_llm(db_session):
    """summarize_pr must work with sync LLMClient.chat (not async).

    Regression: execute() had 'await self.llm.chat(...)' but LLMClient.chat
    is synchronous — only passed tests because mocks used AsyncMock.
    """
    from core.skills.builtin import SummarizePRSkill
    from core.llm.models import LLMProvider, LLMResponse
    from unittest.mock import MagicMock, AsyncMock

    fake_gh = MagicMock()
    fake_gh.get_pr = AsyncMock(return_value={
        "number": 1, "title": "test", "body": "desc",
        "changed_files": 2, "additions": 10, "deletions": 5,
    })
    fake_gh.get_pr_diff = AsyncMock(return_value="diff")

    # Intentionally sync — mimics real LLMClient.chat
    fake_llm = MagicMock()
    fake_llm.chat = MagicMock(return_value=LLMResponse(
        content="Summary", model="gpt-4", provider=LLMProvider.OPENAI,
        tokens_prompt=10, tokens_completion=5, tokens_total=15,
        latency_ms=100, cost_usd=0.001,
    ))

    skill = SummarizePRSkill(fake_llm, fake_gh)
    inp = skill.validate_input({"repo": "owner/repo", "pr_number": 1})
    out = await skill.execute(inp)
    assert out.success is True
    assert out.summary == "Summary"
