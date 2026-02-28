"""E2E tests for 4-area review adoption: data versioning, skill lifecycle, evaluation, SLO."""

import os
import pytest
from datetime import datetime
from sqlalchemy import text
from sqlalchemy.orm import Session

os.environ.setdefault("MATRIXONE_DATABASE", "test_dev_agent_v3")

from api.models import SkillRegistry as SkillRegistryModel


def _session_local():
    from api import database
    return database.SessionLocal


@pytest.fixture
def db_session():
    db = _session_local()()
    yield db
    db.rollback()
    db.close()


# ── Area 1: Data Versioning API ────────────────────────────────────

class TestDataVersioningAPI:
    """Verify time-travel checkpoint and lineage APIs work end-to-end."""

    def test_lineage_trace_upstream(self, db_session):
        """Insert parent→child events, trace upstream returns both."""
        from core.events.event_reader import EventReader
        reader = EventReader(lambda: db_session)

        parent_id = f"ev_parent_{os.urandom(4).hex()}"
        child_id = f"ev_child_{os.urandom(4).hex()}"
        chain_id = f"chain_{os.urandom(4).hex()}"
        sid = f"sess_{os.urandom(4).hex()}"

        for eid, pid in [(parent_id, None), (child_id, parent_id)]:
            db_session.execute(text("""
                INSERT INTO agent_events
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
        reader = EventReader(lambda: db_session)

        chain_id = f"chain_{os.urandom(4).hex()}"
        sid = f"sess_{os.urandom(4).hex()}"
        ids = []
        for i in range(3):
            eid = f"ev_cc_{os.urandom(4).hex()}"
            ids.append(eid)
            db_session.execute(text("""
                INSERT INTO agent_events
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

    @pytest.fixture(autouse=True)
    def _cleanup(self, db_session):
        """Track and remove skills created during each test."""
        self._created_names: list[str] = []
        yield
        for name in self._created_names:
            db_session.execute(
                text("DELETE FROM skills_registry WHERE skill_name = :n"),
                {"n": name},
            )
        db_session.commit()

    def _create_skill(self, db, name, status="active"):
        self._created_names.append(name)
        skill = SkillRegistryModel(
            skill_id=f"sk_{os.urandom(4).hex()}",
            skill_name=name,
            version="1.0.0",
            is_active=1 if status == "active" else 0,
            is_public=1,
            status=status,
            source="marketplace",
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

        registry = SkillRegistry(lambda: db_session)
        registry.publish(name)

        row = db_session.query(SkillRegistryModel).filter_by(skill_name=name).first()
        assert row.status == "active"
        assert row.is_active == 1

    def test_deprecate_transitions_active_to_deprecated(self, db_session):
        """deprecate() moves active skill to deprecated."""
        from core.skills.registry import SkillRegistry
        name = f"sk_dep_{os.urandom(4).hex()}"
        self._create_skill(db_session, name, status="active")

        registry = SkillRegistry(lambda: db_session)
        registry.deprecate(name)

        row = db_session.query(SkillRegistryModel).filter_by(skill_name=name).first()
        assert row.status == "deprecated"
        assert row.is_active == 0

    def test_require_executable_rejects_draft(self, db_session, db_factory):
        """require_executable raises for draft skills."""
        from core.skills.skill_manager import SkillManager, PermissionDeniedError
        from core.skills.credential_manager import CredentialManager
        from config.settings import get_settings

        name = f"sk_draft_{os.urandom(4).hex()}"
        self._create_skill(db_session, name, status="draft")

        mgr = SkillManager(db_factory, CredentialManager(get_settings().secret_key))
        with pytest.raises(PermissionDeniedError, match="draft"):
            mgr.require_executable("any_user", name)

    def test_require_executable_rejects_deprecated(self, db_session, db_factory):
        """require_executable raises for deprecated skills."""
        from core.skills.skill_manager import SkillManager, PermissionDeniedError
        from core.skills.credential_manager import CredentialManager
        from config.settings import get_settings

        name = f"sk_depr_{os.urandom(4).hex()}"
        self._create_skill(db_session, name, status="deprecated")

        mgr = SkillManager(db_factory, CredentialManager(get_settings().secret_key))
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
        monitor = SLOMonitor(lambda: db_session)
        report = monitor.check_agent("nonexistent_agent", period_days=7)
        assert isinstance(report, AgentSLOReport)
        assert report.agent_id == "nonexistent_agent"
        assert len(report.statuses) > 0  # default SLOs exist

    def test_slo_burn_rate_calculation(self, db_session):
        """Burn rate is 0 when no data (no bad days)."""
        from core.evaluation.slo_monitor import SLOMonitor
        monitor = SLOMonitor(lambda: db_session)
        report = monitor.check_agent("empty_agent", period_days=30)
        for s in report.statuses:
            assert s.burn_rate == 0.0

    def test_slo_auto_response_warning(self, db_session):
        """Warning severity writes slo_monitoring_increased event."""
        from core.evaluation.slo_monitor import SLOMonitor, SLOSeverity, SLOStatus, SLOTarget
        from api.database import SessionLocal
        monitor = SLOMonitor(SessionLocal)
        agent_id = f"slo_warn_{os.urandom(4).hex()}"

        status = SLOStatus(
            slo=SLOTarget("quality", "avg_quality", 4.0, ">="),
            current_value=3.5, met=False, burn_rate=2.0,
            severity=SLOSeverity.WARNING, days_elapsed=5, bad_days=3,
        )
        # _auto_respond takes an explicit db session; caller commits.
        db = SessionLocal()
        try:
            monitor._auto_respond(db, agent_id, status)
            db.commit()

            row = db.execute(text("""
                SELECT event_type FROM agent_events
                WHERE agent_id = :aid AND event_type = 'slo_monitoring_increased'
                LIMIT 1
            """), {"aid": agent_id}).fetchone()
            assert row is not None
        finally:
            db.execute(text(
                "DELETE FROM agent_events WHERE agent_id = :aid"
            ), {"aid": agent_id})
            db.commit()
            db.close()

    def test_slo_auto_response_critical_fires_gate(self, db_session):
        """Critical severity calls gate_trigger.trigger() public method."""
        from unittest.mock import MagicMock
        from core.evaluation.slo_monitor import SLOMonitor, SLOSeverity, SLOStatus, SLOTarget

        gate_trigger = MagicMock()
        monitor = SLOMonitor(lambda: db_session, gate_trigger=gate_trigger)
        agent_id = f"slo_crit_{os.urandom(4).hex()}"

        status = SLOStatus(
            slo=SLOTarget("quality", "avg_quality", 4.0, ">="),
            current_value=2.0, met=False, burn_rate=4.0,
            severity=SLOSeverity.CRITICAL, days_elapsed=10, bad_days=8,
        )
        try:
            monitor._auto_respond(db_session, agent_id, status)
            db_session.commit()

            gate_trigger.trigger.assert_called_once()
            kwargs = gate_trigger.trigger.call_args.kwargs
            assert kwargs["change_type"] == "slo_critical"
            # change_id is either recent_change["change_id"] or fallback with agent_id
            assert kwargs["change_id"]  # non-empty
            assert kwargs["change_content"]["agent_id"] == agent_id
        finally:
            db_session.execute(text(
                "DELETE FROM agent_events WHERE agent_id = :aid"
            ), {"aid": agent_id})
            db_session.commit()

    def test_slo_auto_response_breach_creates_postmortem(self, db_session):
        """Breach severity writes post-mortem, model escalation, and HITL tightening events."""
        from core.evaluation.slo_monitor import SLOMonitor, SLOSeverity, SLOStatus, SLOTarget
        from api.database import SessionLocal
        monitor = SLOMonitor(SessionLocal)
        agent_id = f"slo_breach_{os.urandom(4).hex()}"

        status = SLOStatus(
            slo=SLOTarget("quality", "avg_quality", 4.0, ">="),
            current_value=1.5, met=False, burn_rate=10.0,
            severity=SLOSeverity.BREACH, days_elapsed=30, bad_days=25,
        )
        # _auto_respond takes an explicit db; caller commits.
        db = SessionLocal()
        try:
            monitor._auto_respond(db, agent_id, status)
            db.commit()

            rows = db.execute(text("""
                SELECT event_type FROM agent_events
                WHERE agent_id = :aid
                  AND event_type IN ('slo_post_mortem', 'slo_model_escalation', 'slo_hitl_tightened')
                ORDER BY event_type
            """), {"aid": agent_id}).fetchall()
            event_types = {r[0] for r in rows}
            assert "slo_post_mortem" in event_types
            assert "slo_model_escalation" in event_types
            assert "slo_hitl_tightened" in event_types
        finally:
            db.execute(text(
                "DELETE FROM agent_events WHERE agent_id = :aid"
            ), {"aid": agent_id})
            db.commit()
            db.close()

    def test_observability_metrics_query(self, db_session):
        """Observability metrics query runs without error."""
        # Decision layer query
        row = db_session.execute(text("""
            SELECT AVG(quality_score) as avg_quality, COUNT(*) as total
            FROM agent_events
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

    def test_chatloop_slo_escalation(self, db_session):
        """ChatLoop._check_slo_escalation uses ModelRouter.escalate, not hardcoded names."""
        from unittest.mock import MagicMock
        from core.agent.chat_loop import ChatLoop
        from core.llm.router import ModelRouter

        agent_id = f"esc_{os.urandom(4).hex()}"

        # Write escalation event directly
        db_session.execute(text("""
            INSERT INTO agent_events
                (event_id, session_id, user_id, agent_id, agent_version,
                 event_type, content, causal_chain_id, created_at)
            VALUES (:eid, 'system_slo', 'system', :aid, '1.0.0',
                    'slo_model_escalation', '{}', :eid, NOW())
        """), {"eid": os.urandom(8).hex(), "aid": agent_id})
        db_session.commit()

        mock_llm = MagicMock()
        mock_llm.config = {"model": "gpt-4o-mini"}
        router = ModelRouter()
        # Register models (no longer hardcoded defaults)
        from core.llm.router import ModelConfig
        router.register(ModelConfig(model_name="gpt-4o", provider="openai", pricing={"prompt": 0.0025, "completion": 0.01}, fallback_to="gpt-4o-mini"))
        router.register(ModelConfig(model_name="gpt-4o-mini", provider="openai", pricing={"prompt": 0.00015, "completion": 0.0006}))
        mock_llm.router = router
        from contextlib import contextmanager
        mock_el = MagicMock()
        @contextmanager
        def _mock_db():
            yield db_session
        mock_el._db = _mock_db
        chat_loop = ChatLoop(
            selector=MagicMock(), executor=MagicMock(), llm_client=mock_llm,
            event_logger=mock_el, context_manager=MagicMock(),
            firewall=MagicMock(), agent_id=agent_id,
        )

        escalated = chat_loop._check_slo_escalation("s1")
        assert escalated == "gpt-4o"  # gpt-4o.fallback_to == "gpt-4o-mini"
        assert chat_loop._check_slo_escalation("s1") == "gpt-4o"  # cache hit

    def test_hitl_slo_tightening(self, db_session):
        """HITLPolicy appends tightening policy when slo_hitl_tightened event exists."""
        from core.verification.hitl_policy import HITLPolicyEngine, SupervisionAction

        agent_id = f"hitl_{os.urandom(4).hex()}"
        db_session.execute(text("""
            INSERT INTO agent_events
                (event_id, session_id, user_id, agent_id, agent_version,
                 event_type, content, causal_chain_id, created_at)
            VALUES (:eid, 'system_slo', 'system', :aid, '1.0.0',
                    'slo_hitl_tightened', '{}', :eid, NOW())
        """), {"eid": os.urandom(8).hex(), "aid": agent_id})
        db_session.commit()

        engine = HITLPolicyEngine(lambda: db_session)
        engine.load_policies(agent_id)

        names = [p.name for p in engine._policies]
        assert "slo_breach_tightening" in names
        tightening = next(p for p in engine._policies if p.name == "slo_breach_tightening")
        assert tightening.trigger.cost_exceeds == 0.10
        assert tightening.action == SupervisionAction.APPROVE_REJECT


class TestPromptEvolutionGate:
    """Prompt variant promotion must pass regression gate before deploying."""

    @pytest.fixture(autouse=True)
    def ensure_tables(self, db_session):
        """Ensure ctx_prompt_variants table exists (may not be in test DB schema yet)."""
        db_session.execute(text("""
            CREATE TABLE IF NOT EXISTS ctx_prompt_variants (
                variant_id VARCHAR(64) PRIMARY KEY,
                prompt_template_id VARCHAR(64) NOT NULL,
                version INT NOT NULL,
                content TEXT NOT NULL,
                quality_score FLOAT,
                description VARCHAR(255),
                created_at DATETIME DEFAULT NOW()
            )
        """))
        db_session.commit()

    def test_promote_without_gate_succeeds(self, db_session):
        """Without gate, promote_variant updates template directly."""
        from core.evaluation.prompt_evolution import PromptEvolver

        evolver = PromptEvolver(lambda: db_session)
        template_id = f"tmpl_{os.urandom(4).hex()}"
        variant_id = f"var_{os.urandom(4).hex()}"

        # Seed template and variant
        db_session.execute(text("""
            INSERT INTO ctx_prompt_templates (template_id, version, content, is_active, created_at, updated_at)
            VALUES (:tid, 1, 'old content', 1, NOW(), NOW())
        """), {"tid": template_id})
        db_session.execute(text("""
            INSERT INTO ctx_prompt_variants
                (variant_id, prompt_template_id, version, content, quality_score, created_at)
            VALUES (:vid, :tid, 1, 'new content', 4.5, NOW())
        """), {"vid": variant_id, "tid": template_id})
        db_session.commit()

        result = evolver.promote_variant(variant_id, template_id)
        assert result["promoted"] is True

        row = db_session.execute(text(
            "SELECT content FROM ctx_prompt_templates WHERE template_id = :tid"
        ), {"tid": template_id}).fetchone()
        assert row[0] == "new content"

    def test_promote_gate_approved_deploys(self, db_session):
        """Gate approved → variant is promoted."""
        from unittest.mock import MagicMock
        from core.evaluation.prompt_evolution import PromptEvolver

        gate = MagicMock()
        gate.validate_change.return_value = {"verdict": "approved", "metrics": {}}
        evolver = PromptEvolver(lambda: db_session, regression_gate=gate)

        template_id = f"tmpl_{os.urandom(4).hex()}"
        variant_id = f"var_{os.urandom(4).hex()}"
        db_session.execute(text("""
            INSERT INTO ctx_prompt_templates (template_id, version, content, is_active, created_at, updated_at)
            VALUES (:tid, 1, 'old', 1, NOW(), NOW())
        """), {"tid": template_id})
        db_session.execute(text("""
            INSERT INTO ctx_prompt_variants
                (variant_id, prompt_template_id, version, content, quality_score, created_at)
            VALUES (:vid, :tid, 1, 'improved', 4.8, NOW())
        """), {"vid": variant_id, "tid": template_id})
        db_session.commit()

        result = evolver.promote_variant(variant_id, template_id)
        assert result["promoted"] is True
        gate.validate_change.assert_called_once()
        # Verify template was updated
        row = db_session.execute(text(
            "SELECT content FROM ctx_prompt_templates WHERE template_id = :tid"
        ), {"tid": template_id}).fetchone()
        assert row[0] == "improved"

    def test_promote_gate_rejected_blocks(self, db_session):
        """Gate rejected → variant is NOT promoted, template unchanged."""
        from unittest.mock import MagicMock
        from core.evaluation.prompt_evolution import PromptEvolver

        gate = MagicMock()
        gate.validate_change.return_value = {"verdict": "rejected", "reason": "score_regression"}
        evolver = PromptEvolver(lambda: db_session, regression_gate=gate)

        template_id = f"tmpl_{os.urandom(4).hex()}"
        variant_id = f"var_{os.urandom(4).hex()}"
        db_session.execute(text("""
            INSERT INTO ctx_prompt_templates (template_id, version, content, is_active, created_at, updated_at)
            VALUES (:tid, 1, 'original', 1, NOW(), NOW())
        """), {"tid": template_id})
        db_session.execute(text("""
            INSERT INTO ctx_prompt_variants
                (variant_id, prompt_template_id, version, content, quality_score, created_at)
            VALUES (:vid, :tid, 1, 'worse variant', 2.0, NOW())
        """), {"vid": variant_id, "tid": template_id})
        db_session.commit()

        result = evolver.promote_variant(variant_id, template_id)
        assert result["promoted"] is False
        assert result["reason"] == "gate_rejected"
        # Template must be unchanged
        row = db_session.execute(text(
            "SELECT content FROM ctx_prompt_templates WHERE template_id = :tid"
        ), {"tid": template_id}).fetchone()
        assert row[0] == "original"
