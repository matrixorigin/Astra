"""End-to-end replay test: Time travel with skill versioning."""

import pytest

from core.events.event_logger import EventLogger
from core.llm import LLMClient
from core.llm.models import LLMProvider, LLMResponse
from core.replay import ReplayEngine
from core.skills import SkillRegistry
from core.skills.builtin import SummarizePRSkill
from core.skills.github_client import GitHubClient


@pytest.fixture
def db():
    """Database session fixture with cleanup"""
    from api.database import get_db_session
    from sqlalchemy import text
    
    session = next(get_db_session())
    # Cleanup before test
    session.execute(text("DELETE FROM conversation_events WHERE session_id LIKE 'test_session%'"))
    session.execute(text("DELETE FROM skills_registry WHERE skill_name = 'summarize_pr'"))
    session.commit()
    
    yield session
    
    # Cleanup after test
    session.execute(text("DELETE FROM conversation_events WHERE session_id LIKE 'test_session%'"))
    session.execute(text("DELETE FROM skills_registry WHERE skill_name = 'summarize_pr'"))
    session.commit()
    session.close()


@pytest.fixture
def registry(db):
    """Skill registry fixture with cleanup"""
    registry = SkillRegistry(db)
    registry._skills.clear()  # Clear in-memory cache
    return registry


@pytest.fixture
def github(db):
    """GitHub client fixture"""
    return GitHubClient(db)


@pytest.fixture
def llm(db):
    """LLM client fixture"""
    return LLMClient(db)


@pytest.fixture
def logger(db):
    """Event logger fixture"""
    return EventLogger(db)


@pytest.fixture
def replay_engine(db, registry, logger):
    """Replay engine fixture"""
    return ReplayEngine(db, registry, logger)


@pytest.mark.asyncio
async def test_e2e_skill_version_replay(
    db, registry, github, llm, logger, replay_engine, monkeypatch
):
    """End-to-end test: Execute skill v1.0.0, upgrade to v1.1.0, replay with v1.0.0.

    This test verifies the core promise: "10 years later, reproduce today's decision"

    Steps:
    1. Register skill v1.0.0
    2. Execute skill and log to conversation_events
    3. Register skill v1.1.0 (deactivates v1.0.0)
    4. Replay conversation
    5. Verify replay used v1.0.0 (not v1.1.0)
    """

    # Clean up any existing test data
    import time

    session_id = f"test_session_replay_{int(time.time() * 1000)}"  # Use milliseconds for uniqueness

    # Mock LLM response
    async def mock_chat(*args, **kwargs):
        metadata = kwargs.get("metadata", {})
        version = metadata.get("skill_version", "unknown")
        return LLMResponse(
            content=f"Summary from skill version {version}",
            model="gpt-4",
            provider=LLMProvider.OPENAI,
            tokens_prompt=100,
            tokens_completion=50,
            tokens_total=150,
            latency_ms=1000,
            cost_usd=0.002,
        )

    # Mock GitHub API
    async def mock_get_pr(repo_id, pr_number):
        return {
            "number": pr_number,
            "title": f"PR #{pr_number}",
            "body": "Test PR",
            "state": "open",
            "files_changed": 5,
            "additions": 120,
            "deletions": 30,
            "user": "test_user",
            "created_at": "2026-02-10T00:00:00Z",
            "updated_at": "2026-02-10T00:00:00Z",
            "html_url": f"https://github.com/owner/repo/pull/{pr_number}",
        }

    async def mock_get_pr_diff(repo_id, pr_number):
        return "diff --git a/file.py b/file.py\n+test"

    monkeypatch.setattr(llm, "chat", mock_chat)
    monkeypatch.setattr(github, "get_pr", mock_get_pr)
    monkeypatch.setattr(github, "get_pr_diff", mock_get_pr_diff)

    # Step 1: Register skill v1.0.0
    skill_v1 = SummarizePRSkill(llm, github, db)
    skill_v1.version = "1.0.0"
    registry.register(skill_v1)

    # Step 2: Execute skill v1.0.0 and log event
    user_id = "test_user"
    session_id = "test_session_replay"

    input_data = {
        "repo_id": 1,
        "user_id": user_id,
        "session_id": session_id,
        "pr_number": 123,
        "include_diff": True,
    }

    input_obj = skill_v1.validate_input(input_data)
    output_v1 = await skill_v1.execute(input_obj)

    # Log skill execution event (直接插入数据库)
    import json

    from uuid_utils import uuid7

    from api.models import Event
    from datetime import datetime, timezone
    event_id = str(uuid7())
    event = Event(
        event_id=event_id,
        user_id=user_id,
        session_id=session_id,
        agent_id="dev-agent",
        agent_version="0.2.0",
        event_type="skill_exec",
        content=output_v1.summary,
        skill_name=skill_v1.name,
        skill_version=skill_v1.version,
        causal_chain_id=event_id,
        event_metadata={
            "skill": skill_v1.name,
            "skill_version": skill_v1.version,
            "input": input_data,
            "output": output_v1.model_dump(),
        },
        created_at=datetime.now(timezone.utc),
    )
    db.add(event)
    db.commit()

    # Verify v1.0.0 output
    assert "1.0.0" in output_v1.summary

    # Step 3: Register skill v1.1.0 (should deactivate v1.0.0)
    skill_v2 = SummarizePRSkill(llm, github, db)
    skill_v2.version = "1.1.0"
    registry.register(skill_v2)

    # Verify v1.1.0 is now active
    assert registry.get("summarize_pr").version == "1.1.0"

    # Verify v1.0.0 is still available but not active
    assert registry.get("summarize_pr", version="1.0.0") is not None

    # Step 4: Replay conversation
    replay_result = await replay_engine.replay_conversation(session_id)

    # Step 5: Verify replay used v1.0.0 (not v1.1.0)
    assert replay_result["success"] is True
    assert replay_result["events_replayed"] == 1

    replayed_event = replay_result["results"][0]
    assert replayed_event["success"] is True
    assert replayed_event["skill"] == "summarize_pr@1.0.0"
    assert "1.0.0" in replayed_event["output"]["summary"]

    # Verify reproducibility
    verification = replay_engine.verify_reproducibility(session_id)
    assert verification["reproducible"] is True
    assert len(verification["issues"]) == 0

    print("\n✅ E2E Replay Test Passed!")
    print("   - Executed skill v1.0.0")
    print("   - Upgraded to v1.1.0")
    print("   - Replayed with v1.0.0 (not v1.1.0)")
    print("   - Reproducibility verified")


@pytest.mark.asyncio
async def test_replay_missing_skill_version(db, registry, github, llm, logger, replay_engine):
    """Test replay when skill version is missing."""

    # Use unique session ID
    import time

    session_id = f"test_session_missing_{int(time.time() * 1000)}"

    # Create event with non-existent skill version (直接插入数据库)
    import json

    from uuid_utils import uuid7
    from sqlalchemy import text

    user_id = "test_user"

    from api.models import Event
    from datetime import datetime, timezone
    event_id = str(uuid7())
    event = Event(
        event_id=event_id,
        user_id=user_id,
        session_id=session_id,
        agent_id="dev-agent",
        agent_version="0.2.0",
        event_type="skill_exec",
        content="Test content",
        skill_name="nonexistent_skill",
        skill_version="99.99.99",
        causal_chain_id=event_id,
        event_metadata={
            "skill": "nonexistent_skill",
            "skill_version": "99.99.99",
            "input": {"test": "data"},
        },
        created_at=datetime.now(timezone.utc),
    )
    db.add(event)
    db.commit()

    # Replay should handle missing skill gracefully
    replay_result = await replay_engine.replay_conversation(session_id)

    assert replay_result["success"] is True
    assert replay_result["events_replayed"] == 1

    replayed_event = replay_result["results"][0]
    assert replayed_event["success"] is False
    assert "not found" in replayed_event["error"]


@pytest.mark.asyncio
async def test_verify_reproducibility(
    db, registry, github, llm, logger, replay_engine, monkeypatch
):
    """Test reproducibility verification."""

    # Use unique session ID
    import time

    session_id = f"test_session_verify_{int(time.time() * 1000)}"

    # Mock LLM
    async def mock_chat(*args, **kwargs):
        return LLMResponse(
            content="Test summary",
            model="gpt-4",
            provider=LLMProvider.OPENAI,
            tokens_prompt=100,
            tokens_completion=50,
            tokens_total=150,
            latency_ms=1000,
            cost_usd=0.002,
        )

    # Mock GitHub API
    async def mock_get_pr(repo_id, pr_number):
        return {
            "number": pr_number,
            "title": f"PR #{pr_number}",
            "body": "Test PR",
            "state": "open",
            "files_changed": 5,
            "additions": 120,
            "deletions": 30,
            "user": "test_user",
            "created_at": "2026-02-10T00:00:00Z",
            "updated_at": "2026-02-10T00:00:00Z",
            "html_url": f"https://github.com/owner/repo/pull/{pr_number}",
        }

    async def mock_get_pr_diff(repo_id, pr_number):
        return "diff --git a/file.py b/file.py\n+test"

    monkeypatch.setattr(llm, "chat", mock_chat)
    monkeypatch.setattr(github, "get_pr", mock_get_pr)
    monkeypatch.setattr(github, "get_pr_diff", mock_get_pr_diff)

    # Register skill
    skill = SummarizePRSkill(llm, github, db)
    registry.register(skill)

    # Execute skill
    user_id = "test_user"

    input_data = {
        "repo_id": 1,
        "user_id": user_id,
        "session_id": session_id,
        "pr_number": 123,
        "include_diff": True,
    }

    input_obj = skill.validate_input(input_data)
    output = await skill.execute(input_obj)

    # Log event (直接插入数据库)
    import json

    from uuid_utils import uuid7

    from api.models import Event
    from datetime import datetime, timezone
    event_id = str(uuid7())
    event = Event(
        event_id=event_id,
        user_id=user_id,
        session_id=session_id,
        agent_id="dev-agent",
        agent_version="0.2.0",
        event_type="skill_exec",
        content=output.summary,
        skill_name=skill.name,
        skill_version=skill.version,
        causal_chain_id=event_id,
        event_metadata={
            "skill": skill.name,
            "skill_version": skill.version,
            "input": input_data,
        },
        created_at=datetime.now(timezone.utc),
    )
    db.add(event)
    db.commit()

    # Verify reproducibility
    verification = replay_engine.verify_reproducibility(session_id)

    assert verification["reproducible"] is True
    assert verification["events_checked"] == 1
    assert len(verification["issues"]) == 0
