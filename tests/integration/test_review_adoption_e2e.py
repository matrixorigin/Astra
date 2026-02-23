"""E2E tests for 4-area review adoption: data versioning, skill lifecycle, evaluation, SLO."""

import os
import pytest
from datetime import datetime
from sqlalchemy import text
from sqlalchemy.orm import Session

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")

from api.database import SessionLocal
from api.models import SkillDefinition


@pytest.fixture
def db_session():
    db = SessionLocal()
    yield db
    db.rollback()
    db.close()


# ── Area 1: Data Versioning API ────────────────────────────────────

class TestDataVersioningAPI:
    """Verify time-travel checkpoint and lineage APIs work end-to-end."""

    def test_lineage_trace_upstream(self, db_session):
        """Insert parent→child events, trace upstream returns both."""
        from core.events.event_reader import EventReader
        reader = EventReader(db=db_session)

        parent_id = f"ev_parent_{os.urandom(4).hex()}"
        child_id = f"ev_child_{os.urandom(4).hex()}"
        chain_id = f"chain_{os.urandom(4).hex()}"
        sid = f"sess_{os.urandom(4).hex()}"

        for eid, pid in [(parent_id, None), (child_id, parent_id)]:
            db_session.execute(text("""
                INSERT INTO conversation_events
                    (event_id, session_id, user_id, agent_id, agent_version,
                     event_type, content, parent_event_id, causal_chain_id, created_at)
                VALUES (:eid, :sid, 'test', 'test', '1.0', 'user_query', 'test',
                        :pid, :cid, NOW())
            """), {"eid": eid, "sid": sid, "pid": pid, "cid": chain_id})
        db_session.commit()

        # Trace upstream from child — walk parent_event_id chain
        chain = []
        current = child_id
        seen = set()
        while current and current not in seen:
            seen.add(current)
            ev = reader.get_event(current)
            if not ev:
                break
            chain.append(ev)
            current = ev.parent_event_id

        assert len(chain) == 2
        assert chain[0].event_id == child_id
        assert chain[1].event_id == parent_id

    def test_causal_chain_query(self, db_session):
        """Events sharing causal_chain_id are returned together."""
        from core.events.event_reader import EventReader
        reader = EventReader(db=db_session)

        chain_id = f"chain_{os.urandom(4).hex()}"
        sid = f"sess_{os.urandom(4).hex()}"
        ids = []
        for i in range(3):
            eid = f"ev_cc_{os.urandom(4).hex()}"
            ids.append(eid)
            db_session.execute(text("""
                INSERT INTO conversation_events
                    (event_id, session_id, user_id, agent_id, agent_version,
                     event_type, content, causal_chain_id, created_at)
                VALUES (:eid, :sid, 'test', 'test', '1.0', 'user_query', :c, :cid, NOW())
            """), {"eid": eid, "sid": sid, "c": f"msg_{i}", "cid": chain_id})
        db_session.commit()

        chain = reader.get_causal_chain(chain_id)
        returned_ids = {ev.event_id for ev in chain}
        for eid in ids:
            assert eid in returned_ids


# ── Area 2: Skill Lifecycle ────────────────────────────────────────

class TestSkillLifecycle:
    """Verify draft→active→deprecated lifecycle transitions."""

    def _create_skill(self, db, name, status="active"):
        skill = SkillDefinition(
            skill_id=f"sk_{os.urandom(4).hex()}",
            name=name,
            version="1.0.0",
            is_active=1 if status == "active" else 0,
            is_public=1,
            status=status,
            manifest={},
        )
        db.add(skill)
        db.commit()
        return skill

    def test_publish_transitions_draft_to_active(self, db_session):
        """publish() moves draft skill to active."""
        from core.skills.registry import SkillRegistry
        name = f"sk_pub_{os.urandom(4).hex()}"
        self._create_skill(db_session, name, status="draft")

        registry = SkillRegistry(db_session)
        registry.publish(name)

        row = db_session.query(SkillDefinition).filter_by(name=name).first()
        assert row.status == "active"
        assert row.is_active == 1

    def test_deprecate_transitions_active_to_deprecated(self, db_session):
        """deprecate() moves active skill to deprecated."""
        from core.skills.registry import SkillRegistry
        name = f"sk_dep_{os.urandom(4).hex()}"
        self._create_skill(db_session, name, status="active")

        registry = SkillRegistry(db_session)
        registry.deprecate(name)

        row = db_session.query(SkillDefinition).filter_by(name=name).first()
        assert row.status == "deprecated"
        assert row.is_active == 0

    def test_require_executable_rejects_draft(self, db_session):
        """require_executable raises for draft skills."""
        from core.skills.skill_manager import SkillManager, PermissionDeniedError
        from core.skills.credential_manager import CredentialManager
        from config.settings import get_settings

        name = f"sk_draft_{os.urandom(4).hex()}"
        self._create_skill(db_session, name, status="draft")

        mgr = SkillManager(db_session, CredentialManager(get_settings().secret_key))
        with pytest.raises(PermissionDeniedError, match="draft"):
            mgr.require_executable("any_user", name)

    def test_require_executable_rejects_deprecated(self, db_session):
        """require_executable raises for deprecated skills."""
        from core.skills.skill_manager import SkillManager, PermissionDeniedError
        from core.skills.credential_manager import CredentialManager
        from config.settings import get_settings

        name = f"sk_depr_{os.urandom(4).hex()}"
        self._create_skill(db_session, name, status="deprecated")

        mgr = SkillManager(db_session, CredentialManager(get_settings().secret_key))
        with pytest.raises(PermissionDeniedError, match="deprecated"):
            mgr.require_executable("any_user", name)


# ── Area 3: Evaluation Pipeline ───────────────────────────────────

class TestEvaluationPipeline:
    """Verify quality scoring is wired and training data pipeline accessible."""

    def test_auto_scorer_computes_quality(self, db_session):
        """Auto-scorer produces quality_score from firewall result."""
        from core.evaluation.auto_scorer import compute_auto_score
        result = compute_auto_score(
            firewall_passed=True,
            firewall_confidence=0.95,
            response_tokens=200,
        )
        assert result.quality_score > 0
        assert isinstance(result.training_eligible, bool)

    def test_quality_scorer_multi_dimensional(self, db_session):
        """QualityScorer supports multi-dimensional scoring."""
        from core.evaluation.quality_scorer import QualityScorer
        scorer = QualityScorer()
        # Verify multi-dimensional scoring methods exist
        assert hasattr(scorer, "score_accuracy")
        assert hasattr(scorer, "score_latency")
        assert hasattr(scorer, "score_cost")
        assert hasattr(scorer, "score_satisfaction")
        assert hasattr(scorer, "score_trust")


# ── Area 4: SLO Dashboard ─────────────────────────────────────────

class TestSLODashboard:
    """Verify SLO monitoring and dashboard queries work."""

    def test_slo_monitor_check_agent_returns_report(self, db_session):
        """SLOMonitor.check_agent returns structured report even with no data."""
        from core.evaluation.slo_monitor import SLOMonitor, AgentSLOReport
        monitor = SLOMonitor(db_session)
        report = monitor.check_agent("nonexistent_agent", period_days=7)
        assert isinstance(report, AgentSLOReport)
        assert report.agent_id == "nonexistent_agent"
        assert len(report.statuses) > 0  # default SLOs exist

    def test_slo_burn_rate_calculation(self, db_session):
        """Burn rate is 0 when no data (no bad days)."""
        from core.evaluation.slo_monitor import SLOMonitor
        monitor = SLOMonitor(db_session)
        report = monitor.check_agent("empty_agent", period_days=30)
        for s in report.statuses:
            assert s.burn_rate == 0.0

    def test_observability_metrics_query(self, db_session):
        """Observability metrics query runs without error."""
        # Decision layer query
        row = db_session.execute(text("""
            SELECT AVG(quality_score) as avg_quality, COUNT(*) as total
            FROM conversation_events
            WHERE agent_id = 'test_obs' AND event_type = 'llm_response'
              AND created_at > DATE_SUB(NOW(), INTERVAL 7 DAY)
        """)).fetchone()
        assert row is not None
        # Skill layer query
        row = db_session.execute(text("""
            SELECT COUNT(*) as total,
                   SUM(CASE WHEN execution_success = 1 THEN 1 ELSE 0 END) as ok
            FROM skill_selection_events
            WHERE created_at > DATE_SUB(NOW(), INTERVAL 7 DAY)
        """)).fetchone()
        assert row is not None
