"""Tests for skill framework."""

import pytest
from core.skills import (
    SkillRegistry,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
    RepoType,
    AccessScope,
)
from core.skills.builtin import SummarizePRSkill, ListPRsSkill, CIStatusSkill
from core.skills.github_client import GitHubClient
from core.llm import LLMClient
from sdk import Database


@pytest.fixture
def db():
    """Database fixture"""
    return Database()


@pytest.fixture
def registry(db):
    """Skill registry fixture"""
    return SkillRegistry(db)


@pytest.fixture
def github(db):
    """GitHub client fixture"""
    return GitHubClient(db)


@pytest.fixture
def llm(db):
    """LLM client fixture"""
    return LLMClient(db)


def test_skill_registry_register(db, registry, llm, github):
    """Test skill registration"""
    skill = SummarizePRSkill(db, llm, github)
    registry.register(skill)

    # Check in-memory
    assert registry.get("summarize_pr") is not None
    assert registry.get("summarize_pr").version == "1.0.0"

    # Check database
    row = db.fetchone(
        "SELECT * FROM skills_registry WHERE skill_name = %s", ("summarize_pr",)
    )
    assert row is not None
    assert row["version"] == "1.0.0"
    assert row["is_active"] in (1, True, "true")  # Handle different DB return types


def test_skill_registry_versioning(db, registry, llm, github):
    """Test skill versioning"""
    # Register v1.0.0
    skill_v1 = SummarizePRSkill(db, llm, github)
    skill_v1.version = "1.0.0"
    registry.register(skill_v1)

    # Register v1.1.0 (should deactivate v1.0.0)
    skill_v2 = SummarizePRSkill(db, llm, github)
    skill_v2.version = "1.1.0"
    registry.register(skill_v2)

    # Check active version
    assert registry.get("summarize_pr").version == "1.1.0"

    # Check specific version
    assert registry.get("summarize_pr", version="1.0.0") is not None
    assert registry.get("summarize_pr", version="1.1.0") is not None

    # Check database
    rows = db.fetchall(
        "SELECT version, is_active FROM skills_registry WHERE skill_name = %s ORDER BY version",
        ("summarize_pr",),
    )
    assert len(rows) == 2
    assert rows[0]["version"] == "1.0.0"
    assert rows[0]["is_active"] in (0, False, "false")  # Handle different DB return types
    assert rows[1]["version"] == "1.1.0"
    assert rows[1]["is_active"] in (1, True, "true")


def test_skill_registry_list_available(db, registry, llm, github):
    """Test listing available skills for a repo"""
    # Register skills
    registry.register(SummarizePRSkill(db, llm, github))
    registry.register(ListPRsSkill(db, github))
    registry.register(CIStatusSkill(db, github))

    # Create a CODE repo with unique URL
    from core.repos import RepoRegistry, RepoType as RepoTypeEnum, AccessScope as AccessScopeEnum, OwnerType
    import time

    repo_registry = RepoRegistry(db)
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
async def test_summarize_pr_skill(db, llm, github, monkeypatch):
    """Test summarize_pr skill execution"""
    # Mock LLM response
    from core.llm.models import LLMResponse, LLMProvider
    
    async def mock_chat(*args, **kwargs):
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
    
    monkeypatch.setattr(llm, "chat", mock_chat)
    
    skill = SummarizePRSkill(db, llm, github)

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
async def test_list_prs_skill(db, github):
    """Test list_prs skill execution"""
    skill = ListPRsSkill(db, github)

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
async def test_ci_status_skill(db, github):
    """Test ci_status skill execution"""
    skill = CIStatusSkill(db, github)

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
