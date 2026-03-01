"""End-to-end replay test: Skill versioning and session replay via ReplayService."""

import json
from datetime import datetime, timezone

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from api.models import Event
from api.services.replay_service import ReplayService
from core.events.event_logger import EventLogger
from core.llm import LLMClient
from core.llm.models import LLMProvider, LLMResponse
from core.skills import SkillRegistry
from core.skills.builtin import SummarizePRSkill
from core.skills.github_client import GitHubClient


def _uid():
    return str(uuid7())


@pytest.fixture
def session_id():
    return _uid()


@pytest.fixture(autouse=True)
def cleanup(db_session, session_id):
    yield
    try:
        db_session.execute(text(
            "DELETE FROM agent_events WHERE session_id = :sid"
        ), {"sid": session_id})
        db_session.execute(text(
            "DELETE FROM agent_sessions WHERE session_id = :sid"
        ), {"sid": session_id})
        db_session.execute(text(
            "DELETE FROM skills_registry WHERE skill_name = 'summarize_pr'"
        ))
        db_session.commit()
    except Exception:
        db_session.rollback()


@pytest.fixture
def registry(db_session):
    registry = SkillRegistry(lambda: db_session)
    registry._skills.clear()
    return registry


@pytest.fixture
def github(db_session):
    return GitHubClient(db_session)


@pytest.fixture
def llm(db_session):
    return LLMClient(lambda: db_session)


@pytest.fixture
def logger(db_session):
    return EventLogger.from_session(db_session)


@pytest.fixture
def replay_service(db_session):
    return ReplayService(lambda: db_session)


@pytest.mark.asyncio
async def test_e2e_skill_version_replay(
    db_session, registry, github, llm, logger, replay_service, session_id, monkeypatch
):
    """Execute skill v1.0.0, upgrade to v1.1.0, replay uses v1.0.0.

    Core promise: reproduce today's decision 10 years later.
    """
    def mock_chat(*args, **kwargs):
        metadata = kwargs.get("metadata", {})
        version = metadata.get("skill_version", "unknown")
        return LLMResponse(
            content=f"Summary from skill version {version}",
            model="gpt-4", provider=LLMProvider.OPENAI,
            tokens_prompt=100, tokens_completion=50, tokens_total=150,
            latency_ms=1000, cost_usd=0.002,
        )

    async def mock_get_pr(repo_id, pr_number):
        return {
            "number": pr_number, "title": f"PR #{pr_number}", "body": "Test PR",
            "state": "open", "files_changed": 5, "additions": 120, "deletions": 30,
            "user": "test_user", "created_at": "2026-02-10T00:00:00Z",
            "updated_at": "2026-02-10T00:00:00Z",
            "html_url": f"https://github.com/owner/repo/pull/{pr_number}",
        }

    async def mock_get_pr_diff(repo_id, pr_number):
        return "diff --git a/file.py b/file.py\n+test"

    monkeypatch.setattr(llm, "chat", mock_chat)
    monkeypatch.setattr(github, "get_pr", mock_get_pr)
    monkeypatch.setattr(github, "get_pr_diff", mock_get_pr_diff)

    user_id = "test_user"

    # 1. Register skill v1.0.0
    skill_v1 = SummarizePRSkill(llm, github, db_session)
    skill_v1.version = "1.0.0"
    registry.register(skill_v1)

    # 2. Execute and log
    input_data = {
        "repo_id": 1, "user_id": user_id, "session_id": session_id,
        "pr_number": 123, "include_diff": True,
    }
    input_obj = skill_v1.validate_input(input_data)
    output_v1 = await skill_v1.execute(input_obj)
    assert "1.0.0" in output_v1.summary

    # Create session record for ReplayService permission check
    from api.models import Session as SessionModel
    db_session.add(SessionModel(
        session_id=session_id, user_id=user_id, status="active",
    ))

    event_id = _uid()
    db_session.add(Event(
        event_id=event_id, user_id=user_id, session_id=session_id,
        agent_id="dev-agent", agent_version="0.2.0",
        event_type="skill_exec", content=output_v1.summary,
        skill_name=skill_v1.name, skill_version=skill_v1.version,
        causal_chain_id=event_id,
        event_metadata={
            "skill": skill_v1.name, "skill_version": skill_v1.version,
            "input": input_data, "output": output_v1.model_dump(),
        },
        created_at=datetime.now(timezone.utc),
    ))
    db_session.commit()

    # 3. Upgrade to v1.1.0
    skill_v2 = SummarizePRSkill(llm, github, db_session)
    skill_v2.version = "1.1.0"
    registry.register(skill_v2)
    assert registry.get("summarize_pr").version == "1.1.0"
    assert registry.get("summarize_pr", version="1.0.0") is not None

    # 4. Replay via ReplayService
    result = replay_service.replay_session(
        session_id=session_id, user_id=user_id, mock_mode=True,
    )
    assert result["status"] == "completed"
    assert result["events_replayed"] == 1

    # 5. Verify reproducibility
    verification = replay_service.verify_reproducibility(
        session_id=session_id, user_id=user_id,
    )
    assert verification["reproducible"] is True


@pytest.mark.asyncio
async def test_replay_missing_skill_version(db_session, replay_service, session_id):
    """Replay handles missing skill gracefully."""
    user_id = "test_user"

    from api.models import Session as SessionModel
    db_session.add(SessionModel(
        session_id=session_id, user_id=user_id, status="active",
    ))

    event_id = _uid()
    db_session.add(Event(
        event_id=event_id, user_id=user_id, session_id=session_id,
        agent_id="dev-agent", agent_version="0.2.0",
        event_type="skill_exec", content="Test content",
        skill_name="nonexistent_skill", skill_version="99.99.99",
        causal_chain_id=event_id,
        event_metadata={"skill": "nonexistent_skill", "skill_version": "99.99.99"},
        created_at=datetime.now(timezone.utc),
    ))
    db_session.commit()

    result = replay_service.replay_session(
        session_id=session_id, user_id=user_id, mock_mode=True,
    )
    assert result["status"] == "completed"
    # Non-skill events replay as passthrough (success)
    assert result["events_replayed"] == 1
