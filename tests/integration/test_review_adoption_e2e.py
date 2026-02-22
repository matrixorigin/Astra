"""E2E tests for review adoption changes.

Realistic scenarios with seeded data — not just "doesn't crash on empty".

  - Skill lifecycle: draft → active → deprecated → archived with real gate trigger
  - Memory pipeline: seeded observations + knowledge → pipeline detects and acts
  - Trust report: seeded llm_response events with quality/confidence → real scores
  - Memory health: seeded observations + knowledge → real counts
  - AgentRun priority: serialization round-trip
"""

from __future__ import annotations

import json
from datetime import datetime, timedelta
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text

from core.skills.base import (
    AccessScope, RepoType, SideEffectCategory, SideEffectProfile,
    Skill, SkillRequirement,
)
from core.skills.registry import SkillRegistry
from core.utils.id_generator import generate_id


# ── Helpers ──────────────────────────────────────────────────────────


class StubSkill(Skill):
    def __init__(self, name: str, version: str):
        self.name = name
        self.version = version
        self.description = f"stub {name}"
        self.requirements = SkillRequirement(
            repo_types=[RepoType.CODE], min_access=AccessScope.READ,
        )
        self.side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ)

    def validate_input(self, input_data: dict):
        return input_data

    async def execute(self, **kwargs):
        return {"ok": True}


def _seed_llm_events(db, agent_id: str, n: int = 10, quality: float = 4.0):
    """Insert llm_response events with quality_score and confidence metadata."""
    for i in range(n):
        db.execute(text("""
            INSERT INTO conversation_events
                (event_id, session_id, user_id, agent_id, agent_version,
                 event_type, content, quality_score, `metadata`,
                 causal_chain_id, created_at)
            VALUES (:eid, :sid, 'system', :aid, '1.0.0',
                    'llm_response', :content, :qs, :meta, :eid, :ts)
        """), {
            "eid": generate_id(),
            "sid": f"trust_{generate_id()[:8]}",
            "aid": agent_id,
            "content": f"Response about topic {i}",
            "qs": quality - (i % 3) * 0.5,  # vary: 4.0, 3.5, 3.0, 4.0, ...
            "meta": json.dumps({"confidence_score": 0.8 + (i % 3) * 0.05}),
            "ts": datetime.now() - timedelta(days=i % 5),
        })
    db.commit()


def _seed_observations(db, user_id: str, n: int = 5, reflected: int = 2):
    """Insert observations, some reflected, some pending."""
    for i in range(n):
        db.execute(text("""
            INSERT INTO observations
                (observation_id, user_id, session_id, content, priority,
                 observation_type, observed_at, source_event_ids,
                 is_reflected, confidence, created_at)
            VALUES (:oid, :uid, :sid, :content, 'medium',
                    'preference', NOW(), :sources,
                    :reflected, 0.75, NOW())
        """), {
            "oid": generate_id(),
            "uid": user_id,
            "sid": f"obs_sess_{i}",
            "content": f"User prefers concise answers (observation {i})",
            "sources": json.dumps([generate_id()]),
            "reflected": 1 if i < reflected else 0,
        })
    db.commit()


def _seed_knowledge(db, user_id: str, n: int = 4):
    """Insert knowledge entries — some healthy, some low-confidence, some quarantined."""
    states = [
        (0.9, "T1"),   # healthy, high confidence
        (0.7, "T2"),   # healthy, medium confidence
        (0.2, "T3"),   # low confidence (< 0.3)
        (0.0, "T3"),   # quarantined (confidence = 0)
    ]
    for i in range(min(n, len(states))):
        conf, tier = states[i]
        db.execute(text("""
            INSERT INTO sk_knowledge_entries
                (entry_id, user_id, key_name, category, value,
                 confidence, trust_tier, source_event_ids, created_at)
            VALUES (:eid, :uid, :key, 'preference', :val,
                    :conf, :tier, :sources, NOW())
        """), {
            "eid": generate_id(),
            "uid": user_id,
            "key": f"pref_{i}",
            "val": f"Knowledge entry {i}",
            "conf": conf,
            "tier": tier,
            "sources": json.dumps([generate_id()]),
        })
    db.commit()


# ── Fixtures ─────────────────────────────────────────────────────────


@pytest.fixture
def db():
    from api.database import get_db_session
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def registry(db):
    return SkillRegistry(db)


@pytest.fixture
def client():
    from api.main import app
    return TestClient(app)


@pytest.fixture
def auth_headers(client):
    username = f"review_{generate_id()[:12]}"
    client.post("/auth/register", json={
        "username": username,
        "email": f"{username}@test.com",
        "password": "testpass1234",
    })
    resp = client.post("/auth/login", json={
        "username": username, "password": "testpass1234",
    })
    return {"Authorization": f"Bearer {resp.json()['access_token']}"}


# ============================================================================
# 1. Skill Lifecycle — real DB, real gate trigger mock
# ============================================================================

class TestSkillLifecycle:

    def test_full_lifecycle_draft_to_archived(self, registry, db):
        """Realistic scenario: team registers draft skill, QA promotes to active,
        later deprecates old version when new one ships, eventually archives."""
        name = f"code_review_{generate_id()[:8]}"
        gate = MagicMock()
        registry.gate_trigger = gate
        from api.models import SkillRegistry as SkillModel

        # 1. Dev registers skill as draft — no gate, no activation
        registry.register(StubSkill(name, "1.0.0"), is_active=True, status="draft")
        gate.on_skill_change.assert_not_called()
        row = db.query(SkillModel).filter(SkillModel.skill_id == f"{name}@1.0.0").first()
        assert row.status == "draft"
        assert row.is_active == 0

        # 2. QA promotes to active — gate fires, skill becomes queryable
        registry.set_status(name, "1.0.0", "active")
        gate.on_skill_change.assert_called_once_with(
            skill_name=name, version="1.0.0", definition=row.skill_definition or {},
        )
        db.refresh(row)
        assert row.is_active == 1

        # 3. Dev ships v2 as draft while v1 is active
        registry.register(StubSkill(name, "2.0.0"), is_active=True, status="draft")
        db.refresh(row)
        assert row.is_active == 1  # v1 still active — draft v2 didn't touch it

        # 4. Promote v2 → active: v1 must be deactivated
        gate.reset_mock()
        registry.set_status(name, "2.0.0", "active")
        v1 = db.query(SkillModel).filter(SkillModel.skill_id == f"{name}@1.0.0").first()
        v2 = db.query(SkillModel).filter(SkillModel.skill_id == f"{name}@2.0.0").first()
        db.refresh(v1)
        db.refresh(v2)
        assert v1.is_active == 0, "v1 must be deactivated when v2 activates"
        assert v2.is_active == 1
        gate.on_skill_change.assert_called_once()

        # 5. Deprecate v1, then archive
        registry.set_status(name, "1.0.0", "deprecated")
        registry.set_status(name, "1.0.0", "archived")
        db.refresh(v1)
        assert v1.status == "archived"

    def test_invalid_transitions_rejected(self, registry, db):
        """Cannot skip states: active→archived, archived→active."""
        name = f"guard_{generate_id()[:8]}"
        registry.register(StubSkill(name, "1.0.0"), status="active")

        with pytest.raises(ValueError, match="Invalid transition"):
            registry.set_status(name, "1.0.0", "draft")

        with pytest.raises(ValueError, match="Invalid transition"):
            registry.set_status(name, "1.0.0", "archived")

        # Deprecate then archive — valid path
        registry.set_status(name, "1.0.0", "deprecated")
        registry.set_status(name, "1.0.0", "archived")

        # Archived is terminal
        with pytest.raises(ValueError, match="Invalid transition"):
            registry.set_status(name, "1.0.0", "active")

    def test_reactivate_deprecated_skill(self, registry, db):
        """deprecated → active is allowed (rollback scenario)."""
        name = f"rollback_{generate_id()[:8]}"
        from api.models import SkillRegistry as SkillModel

        registry.register(StubSkill(name, "1.0.0"), status="active")
        registry.set_status(name, "1.0.0", "deprecated")
        registry.set_status(name, "1.0.0", "active")

        row = db.query(SkillModel).filter(SkillModel.skill_id == f"{name}@1.0.0").first()
        db.refresh(row)
        assert row.status == "active"
        assert row.is_active == 1

    def test_nonexistent_skill_returns_false(self, registry):
        assert registry.set_status("ghost", "0.0.0", "active") is False

    def test_register_invalid_status_raises(self, registry):
        with pytest.raises(ValueError, match="Invalid skill status"):
            registry.register(StubSkill("x", "1.0.0"), status="bogus")


# ============================================================================
# 2. Memory Pipeline — seeded data, mocked LLM for observer
# ============================================================================

class TestMemoryPipeline:

    def test_pipeline_extracts_observations_with_mock_llm(self, db):
        """Full pipeline: mock LLM → Observer extracts → Reflector sees them → no errors."""
        from core.memory.pipeline import run_memory_pipeline

        user_id = f"mem_{generate_id()}"
        session_id = generate_id()

        # Seed a conversation with enough content to exceed observer threshold (2000 tokens)
        # Realistic: a 10-turn technical discussion
        turns = [
            ("user_message", "I'm building a fintech application and need help choosing between Python and Go for the backend. We handle about 10,000 transactions per second and need sub-100ms latency. Security is critical since we process payment data. We're currently using a monolithic Django app but considering microservices."),
            ("llm_response", "For your fintech use case with 10K TPS and sub-100ms latency requirements, both languages are viable but serve different strengths. Python with FastAPI can handle high throughput with async, while Go excels at raw performance. Given your security requirements for payment processing, I'd recommend a hybrid approach."),
            ("user_message", "That's interesting. We also need to comply with PCI DSS and SOC 2. Our team is mostly Python developers with 5 years experience. We tried Go last year but the learning curve was steep. We use PostgreSQL and Redis currently, and we're considering adding Kafka for event streaming."),
            ("llm_response", "Given your team's Python expertise and compliance requirements, I'd suggest staying with Python for most services. FastAPI with Pydantic gives you strong type safety and automatic validation which helps with PCI DSS compliance. For the few latency-critical paths like payment processing, you could write those specific services in Go."),
            ("user_message", "Good point about the hybrid approach. We also need comprehensive audit logging for every transaction. Currently we log to files but need something more structured. We're evaluating between ELK stack and a custom event sourcing solution. What would you recommend for a team of 15 engineers?"),
            ("llm_response", "For audit logging in fintech with PCI DSS requirements, I'd strongly recommend event sourcing over simple log aggregation. Event sourcing gives you immutable audit trails, point-in-time reconstruction, and natural compliance documentation. The ELK stack is great for operational logs but event sourcing is better for business-critical audit trails."),
            ("user_message", "We've decided to go with event sourcing. Now I need help with the data model. Each transaction event should capture: amount, currency, sender, receiver, timestamp, and the full context of why the transaction was approved or rejected. We also need to handle idempotency for retries."),
            ("llm_response", "For your event sourcing data model, I'd suggest: 1) An Event table with event_id (UUID), aggregate_id, event_type, payload (JSON), metadata, created_at. 2) Use the aggregate_id as the transaction ID for grouping related events. 3) For idempotency, add an idempotency_key column with a unique constraint. 4) Store the approval/rejection context in the payload JSON."),
            ("user_message", "Perfect. One more thing - we need to handle multi-currency transactions. Some transactions involve currency conversion with rates that change every minute. How should we snapshot the exchange rate at transaction time to ensure our audit trail is accurate?"),
            ("llm_response", "For multi-currency audit accuracy, snapshot the exchange rate as part of the event payload at transaction time. Store: source_currency, target_currency, exchange_rate, rate_provider, rate_timestamp. Never recalculate from current rates - the snapshot IS the truth. This is critical for PCI DSS and financial reconciliation."),
        ]
        for i, (etype, content) in enumerate(turns):
            db.execute(text("""
                INSERT INTO conversation_events
                    (event_id, session_id, user_id, agent_id, agent_version,
                     event_type, content, causal_chain_id, created_at)
                VALUES (:eid, :sid, :uid, 'dev-agent', '1.0',
                        :etype, :content, :chain, NOW())
            """), {
                "eid": generate_id(), "sid": session_id, "uid": user_id,
                "etype": etype, "content": content, "chain": generate_id(),
            })
        db.commit()

        # Mock LLM that returns structured observations (what a real LLM would extract)
        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"content": "User works at a fintech company processing payments", "priority": "high", "type": "fact"},
                {"content": "Team has 15 Python engineers with 5 years experience", "priority": "medium", "type": "fact"},
                {"content": "System requires sub-100ms latency and 10K TPS", "priority": "high", "type": "preference"},
                {"content": "Must comply with PCI DSS and SOC 2", "priority": "high", "type": "fact"},
                {"content": "Chose event sourcing for audit logging", "priority": "medium", "type": "decision"},
            ])
        }

        result = run_memory_pipeline(
            db, user_id=user_id, session_id=session_id,
            llm_client=mock_llm, observe_threshold=100,
        )

        # Observer should have extracted observations via LLM
        assert result.observations_extracted == 5
        assert mock_llm.chat_with_tools.call_count >= 1
        assert len(result.errors) == 0

        # Verify observations actually persisted in DB
        from api.models import Observation
        obs = db.query(Observation).filter(Observation.user_id == user_id).all()
        assert len(obs) == 5
        contents = {o.content for o in obs}
        assert "User works at a fintech company processing payments" in contents
        assert any(o.priority == "high" for o in obs)

    def test_pipeline_no_data_no_errors(self, db):
        """Empty user — pipeline returns zeros, no exceptions."""
        from core.memory.pipeline import run_memory_pipeline

        result = run_memory_pipeline(db, user_id=f"empty_{generate_id()}")
        assert result.observations_extracted == 0
        assert result.contradictions_found == 0
        assert result.errors == []


# ============================================================================
# 3. Trust Report API — seeded events → real scores
# ============================================================================

class TestTrustReportAPI:

    def test_trust_report_with_seeded_data(self, client, auth_headers, db):
        """Seed llm_response events with quality + confidence, verify report computes real scores."""
        agent_id = f"trust_agent_{generate_id()[:8]}"
        _seed_llm_events(db, agent_id, n=15, quality=4.2)

        resp = client.get(
            f"/api/v1/evaluation/trust-report?agent_id={agent_id}&days=7",
            headers=auth_headers,
        )
        assert resp.status_code == 200
        body = resp.json()

        # Overall score should be computed from real data
        assert 0.0 < body["overall_trust_score"] <= 1.0

        # Confidence calibration — seeded events have confidence_score + quality_score
        assert body["confidence_calibration"] is not None
        assert body["confidence_calibration"]["sample_count"] == 15
        assert 0.0 <= body["confidence_calibration"]["calibration_error"] <= 1.0

        # SLO — seeded events have quality_score, SLO monitor should evaluate
        assert body["slo_summary"] is not None
        assert body["slo_summary"]["total_slos"] > 0
        assert 0.0 <= body["slo_summary"]["compliance_rate"] <= 1.0

        # Drift detector always runs
        if body["drift_summary"]:
            assert "total_signals" in body["drift_summary"]

    def test_trust_report_empty_agent(self, client, auth_headers):
        """Unknown agent — should return 200 with a valid score, not crash."""
        resp = client.get(
            "/api/v1/evaluation/trust-report?agent_id=nonexistent_agent&days=7",
            headers=auth_headers,
        )
        assert resp.status_code == 200
        body = resp.json()
        assert 0.0 <= body["overall_trust_score"] <= 1.0


# ============================================================================
# 4. Memory Health API — seeded observations + knowledge → real counts
# ============================================================================

class TestMemoryHealthAPI:

    def test_memory_health_with_seeded_data(self, client, auth_headers, db):
        """Seed observations + knowledge entries, verify health endpoint returns real counts."""
        user_id = f"health_{generate_id()}"  # full ID to avoid collisions
        _seed_observations(db, user_id, n=5, reflected=2)
        _seed_knowledge(db, user_id, n=4)

        resp = client.get(
            f"/api/v1/evaluation/memory-health?user_id={user_id}",
            headers=auth_headers,
        )
        assert resp.status_code == 200
        body = resp.json()

        # Observations: 5 total, 3 pending reflection
        assert body["observations"]["total"] == 5
        assert body["observations"]["pending_reflection"] == 3

        # Knowledge: 4 total, 2 low confidence (<0.3 includes 0.2 and 0.0), 1 quarantined (conf=0)
        assert body["knowledge"]["total"] == 4
        assert body["knowledge"]["low_confidence"] == 2
        assert body["knowledge"]["quarantined"] == 1

    def test_memory_health_empty_user(self, client, auth_headers):
        """Unknown user — empty dicts, not errors."""
        resp = client.get(
            "/api/v1/evaluation/memory-health?user_id=nobody_here",
            headers=auth_headers,
        )
        assert resp.status_code == 200


# ============================================================================
# 5. AgentRun Priority
# ============================================================================

class TestAgentRunPriority:

    def test_default_priority(self):
        from core.agent.run import AgentRun
        run = AgentRun(agent_id="test", user_id="u1", session_id="s1")
        assert run.priority == 5

    def test_custom_priority_round_trip(self):
        from core.agent.run import AgentRun
        run = AgentRun(agent_id="test", user_id="u1", session_id="s1", priority=0)
        assert run.priority == 0
        assert run.model_dump()["priority"] == 0

    def test_priority_ordering(self):
        """Higher priority (lower number) runs should sort first."""
        from core.agent.run import AgentRun
        runs = [
            AgentRun(agent_id="a", user_id="u", session_id="s", priority=p)
            for p in [5, 0, 10, 3]
        ]
        sorted_runs = sorted(runs, key=lambda r: r.priority)
        assert [r.priority for r in sorted_runs] == [0, 3, 5, 10]
