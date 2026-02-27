"""End-to-end round-trip test: record in PRODUCTION → replay in REPLAY.

Verifies the core value proposition of the replay system:
skill execution results recorded during production can be faithfully
replayed without re-execution.
"""

import json

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from api.models import Event
from core.exceptions import ReplayError
from core.skills.base import (
    AccessScope, RepoType, SideEffectCategory, SideEffectProfile,
    Skill, SkillInput, SkillOutput, SkillRequirement,
)
from core.skills.mocking import MockMode, SecurityError, ToolMockingLayer


# ── Mock skills ───────────────────────────────────────────

class ReadInput(SkillInput):
    query: str

class ReadOutput(SkillOutput):
    data: str

class MockReadSkill(Skill):
    name = "mock_read"
    version = "1.0.0"
    description = "Read skill for testing"
    requirements = SkillRequirement(repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False)
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ, external_apis=[])

    def validate_input(self, input_data: dict) -> ReadInput:
        return ReadInput(**input_data)

    def execute(self, input: ReadInput) -> ReadOutput:
        return ReadOutput(success=True, result=f"read:{input.query}", data=f"read:{input.query}")


class MockWriteSkill(Skill):
    name = "mock_write"
    version = "1.0.0"
    description = "Write skill for testing"
    requirements = SkillRequirement(repo_types=[RepoType.CODE], min_access=AccessScope.WRITE, llm_required=False)
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.WRITE, external_apis=["github"])

    def validate_input(self, input_data: dict) -> ReadInput:
        return ReadInput(**input_data)

    def execute(self, input: ReadInput) -> ReadOutput:
        return ReadOutput(success=True, result=f"write:{input.query}", data=f"write:{input.query}")


class MockDestructiveSkill(Skill):
    name = "mock_destroy"
    version = "1.0.0"
    description = "Destructive skill for testing"
    requirements = SkillRequirement(repo_types=[RepoType.CODE], min_access=AccessScope.WRITE, llm_required=False)
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.DESTRUCTIVE, external_apis=[])

    def validate_input(self, input_data: dict) -> ReadInput:
        return ReadInput(**input_data)

    def execute(self, input: ReadInput) -> ReadOutput:
        return ReadOutput(success=True, result="destroyed", data="destroyed")


# ── Fixtures ──────────────────────────────────────────────

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
        db_session.commit()
    except Exception:
        db_session.rollback()


def _create_tool_result_event(db, session_id, skill_name, skill_version, params, parent_event_id=None):
    """Insert a tool_result event that ToolMockingLayer can find."""
    eid = _uid()
    db.add(Event(
        event_id=eid,
        session_id=session_id,
        user_id="test_user",
        event_type="tool_result",
        content="",
        skill_name=skill_name,
        skill_version=skill_version,
        parent_event_id=parent_event_id,
        causal_chain_id=_uid(),
        event_metadata={
            "skill_params": params,
            "skill_result": {"data": f"recorded:{params.get('query', '')}"},
        },
    ))
    db.commit()
    return eid


# ── Tests ─────────────────────────────────────────────────

class TestProductionRecordThenReplay:
    """Record via PRODUCTION mode, replay via REPLAY mode."""

    def test_write_skill_round_trip(self, db_session, session_id):
        """PRODUCTION records result → REPLAY returns same result."""
        skill = MockWriteSkill()
        params = {"query": "hello"}
        parent_eid = _uid()

        # Create the event that _record_result will update
        db_session.add(Event(
            event_id=_uid(),
            session_id=session_id,
            user_id="test_user",
            event_type="tool_result",
            content="",
            skill_name="mock_write",
            skill_version="1.0.0",
            parent_event_id=parent_eid,
            causal_chain_id=_uid(),
            event_metadata={},
        ))
        db_session.commit()

        # Record in PRODUCTION
        prod = ToolMockingLayer(mode=MockMode.PRODUCTION, db_factory=lambda: db_session)
        result = prod.execute(skill, params, session_id, parent_event_id=parent_eid)
        assert result.data == "write:hello"

        # Replay in REPLAY
        replay = ToolMockingLayer(mode=MockMode.REPLAY, db_factory=lambda: db_session, session_id=session_id)
        replayed = replay.get_mock_result("mock_write", params, session_id, parent_event_id=parent_eid)
        assert replayed is not None
        assert replayed["data"] == "write:hello"


class TestReplayBlocksDestructive:
    """DESTRUCTIVE skills raise SecurityError in REPLAY mode."""

    def test_destructive_blocked(self, db_session, session_id):
        skill = MockDestructiveSkill()
        replay = ToolMockingLayer(mode=MockMode.REPLAY, db_factory=lambda: db_session, session_id=session_id)
        with pytest.raises(SecurityError, match="Blocked destructive"):
            replay.execute(skill, {"query": "x"}, session_id)


class TestReplayWithVersionMismatch:
    """Replay warns when recorded version differs from current skill version."""

    def test_version_mismatch_warns(self, db_session, session_id, caplog):
        """Record with v1.0.0, replay lookup with v2.0.0 → warning logged."""
        params = {"query": "test"}
        parent_eid = _uid()

        # Create recorded event with v1.0.0
        db_session.add(Event(
            event_id=_uid(),
            session_id=session_id,
            user_id="test_user",
            event_type="tool_result",
            content="",
            skill_name="mock_read",
            skill_version="1.0.0",
            parent_event_id=parent_eid,
            causal_chain_id=_uid(),
            event_metadata={
                "skill_params": params,
                "skill_result": {"data": "v1-result"},
                "skill_version": "1.0.0",
            },
        ))
        db_session.commit()

        replay = ToolMockingLayer(mode=MockMode.REPLAY, db_factory=lambda: db_session, session_id=session_id)
        result = replay._get_recorded_result(
            "mock_read", params, session_id, parent_eid,
            expected_version="2.0.0",
        )
        # Should still return the result but with a warning
        assert result is not None
        assert result["data"] == "v1-result"
        assert any("version mismatch" in r.message.lower() for r in caplog.records)


class TestReplayServiceFullSession:
    """ReplayService replays a full session and compares outputs."""

    def test_replay_and_compare(self, db_session, session_id):
        from api.services.replay_service import ReplayService
        from api.models import Session as SessionModel

        user_id = "test_user"

        # Create session
        db_session.add(SessionModel(
            session_id=session_id, user_id=user_id, status="active",
        ))

        # Create events
        for i in range(3):
            db_session.add(Event(
                event_id=_uid(),
                session_id=session_id,
                user_id=user_id,
                event_type="user_query" if i % 2 == 0 else "llm_response",
                content=f"content_{i}",
                causal_chain_id=_uid(),
            ))
        db_session.commit()

        svc = ReplayService(lambda: db_session)

        # Replay
        result = svc.replay_session(session_id=session_id, user_id=user_id, mock_mode=True)
        assert result["status"] == "completed"
        assert result["events_replayed"] == 3
        assert result["result"]["successful"] == 3

        # Compare
        comparison = svc.compare_outputs(
            session_id=session_id, user_id=user_id,
            replay_result=result["result"],
        )
        assert comparison["match"] is True
        assert comparison["mismatched_events"] == 0
