"""Integration tests for evaluation pipeline — real DB, no mocks.

Tests the full Step → Chain → Session scoring pipeline and the evaluation API.
"""

import pytest
from sqlalchemy import text

from api.database import get_db_session
from core.evaluation.auto_scorer import compute_auto_score
from core.evaluation.multi_level_scorer import score_chain, score_session
from core.utils.id_generator import generate_id


@pytest.fixture
def db():
    return next(get_db_session())


@pytest.fixture
def session_id():
    return generate_id()


@pytest.fixture
def user_id():
    return generate_id()


def _insert_event(db, *, session_id, user_id, chain_id, quality_score=None,
                  event_type="llm_response", training_eligible=False):
    """Insert a minimal conversation_events row for testing."""
    eid = generate_id()
    db.execute(text("""
        INSERT INTO conversation_events
        (event_id, session_id, user_id, agent_id, agent_version,
         event_type, content, causal_chain_id, quality_score, training_eligible, created_at)
        VALUES (:eid, :sid, :uid, 'test-agent', '1.0',
                :etype, 'test content', :cid, :qs, :te, NOW())
    """), {
        "eid": eid, "sid": session_id, "uid": user_id,
        "cid": chain_id, "etype": event_type,
        "qs": quality_score, "te": int(training_eligible),
    })
    db.commit()
    return eid


class TestAutoScorer:
    """Unit-level: compute_auto_score is pure function, no DB needed."""

    def test_high_quality(self):
        r = compute_auto_score(
            firewall_passed=True, firewall_confidence=0.95,
            response_tokens=200,
        )
        assert r.quality_score >= 4.0
        assert r.training_eligible is True

    def test_low_confidence(self):
        r = compute_auto_score(
            firewall_passed=False, firewall_confidence=0.2,
            response_tokens=200,
        )
        assert r.quality_score < 3.0
        assert r.training_eligible is False


class TestScoreChain:
    """Chain-level scoring with real DB writes."""

    def test_single_step(self, db, session_id, user_id):
        chain_id = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=4.5)

        result = score_chain(db, chain_id, session_id)
        assert result is not None
        assert result["step_count"] == 1
        assert result["score"] == pytest.approx(4.5, abs=0.01)

        # Verify persisted to quality_assessments
        row = db.execute(text(
            "SELECT score, level FROM quality_assessments "
            "WHERE target_id = :tid AND level = 'chain'"
        ), {"tid": chain_id}).fetchone()
        assert row is not None
        assert float(row.score) == pytest.approx(4.5, abs=0.01)

    def test_cascade_penalty(self, db, session_id, user_id):
        """Low-quality step penalises chain score."""
        chain_id = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=4.5)
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=1.0)  # bad step

        result = score_chain(db, chain_id, session_id)
        assert result is not None
        assert result["step_count"] == 2
        assert result["failure_count"] == 1
        # Score should be lower than simple average due to cascade penalty
        simple_avg = (4.5 + 1.0) / 2
        assert result["score"] < simple_avg

    def test_no_scored_events(self, db, session_id, user_id):
        chain_id = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=None)

        result = score_chain(db, chain_id, session_id)
        assert result is None

    def test_upsert_updates_existing(self, db, session_id, user_id):
        """Second score_chain call updates, not duplicates."""
        chain_id = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=3.0)

        r1 = score_chain(db, chain_id, session_id)
        assert r1 is not None

        # Add another event and re-score
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=5.0)
        r2 = score_chain(db, chain_id, session_id)
        assert r2 is not None
        assert r2["step_count"] == 2
        assert r2["score"] != r1["score"]

        # Should still be exactly 1 row
        count = db.execute(text(
            "SELECT COUNT(*) FROM quality_assessments "
            "WHERE target_id = :tid AND level = 'chain'"
        ), {"tid": chain_id}).fetchone()[0]
        assert count == 1


class TestScoreSession:
    """Session-level scoring from chain assessments."""

    def test_single_chain(self, db, session_id, user_id):
        chain_id = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=4.0)
        score_chain(db, chain_id, session_id)

        result = score_session(db, session_id)
        assert result is not None
        assert result["chain_count"] == 1
        assert result["score"] == pytest.approx(4.0, abs=0.01)

    def test_multi_chain_weighted(self, db, session_id, user_id):
        """Longer chains weigh more in session score."""
        # Chain 1: 1 step, score ~4.5
        c1 = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=c1, quality_score=4.5)
        score_chain(db, c1, session_id)

        # Chain 2: 2 steps, score ~2.0
        c2 = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=c2, quality_score=2.0)
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=c2, quality_score=2.0)
        score_chain(db, c2, session_id)

        result = score_session(db, session_id)
        assert result is not None
        assert result["chain_count"] == 2
        # Chain 2 has 2 steps (weight 2) vs chain 1 with 1 step (weight 1)
        # So session score should be closer to chain 2's score
        assert result["score"] < 3.5  # weighted toward the longer, lower chain

    def test_no_chains(self, db):
        result = score_session(db, generate_id())
        assert result is None

    def test_upsert_updates_existing(self, db, session_id, user_id):
        """Re-scoring session updates existing row."""
        c1 = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=c1, quality_score=3.0)
        score_chain(db, c1, session_id)
        score_session(db, session_id)

        # Add another chain and re-score
        c2 = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=c2, quality_score=5.0)
        score_chain(db, c2, session_id)
        score_session(db, session_id)

        count = db.execute(text(
            "SELECT COUNT(*) FROM quality_assessments "
            "WHERE target_id = :tid AND level = 'session'"
        ), {"tid": session_id}).fetchone()[0]
        assert count == 1


class TestEvaluationAPI:
    """Test evaluation API endpoints via TestClient."""

    @pytest.fixture
    def client(self):
        from fastapi.testclient import TestClient
        from api.main import app
        return TestClient(app)

    def test_quality_trend(self, client, db, session_id, user_id):
        # Insert scored events
        chain_id = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=4.0,
                      training_eligible=True)

        resp = client.get("/api/v1/evaluation/quality/trend", params={"days": 1})
        assert resp.status_code == 200
        data = resp.json()
        assert "points" in data
        assert data["total_events"] >= 1

    def test_drift_returns_list(self, client):
        resp = client.get("/api/v1/evaluation/drift")
        assert resp.status_code == 200
        assert isinstance(resp.json(), list)

    def test_gates_returns_list(self, client):
        resp = client.get("/api/v1/evaluation/gates")
        assert resp.status_code == 200
        assert isinstance(resp.json(), list)

    def test_calibration(self, client):
        resp = client.get("/api/v1/evaluation/calibration")
        assert resp.status_code == 200
        data = resp.json()
        assert "calibration_error" in data
        assert "adjustment_multiplier" in data

    def test_session_scores(self, client, db, session_id, user_id):
        # Build a scored session
        chain_id = generate_id()
        _insert_event(db, session_id=session_id, user_id=user_id,
                      chain_id=chain_id, quality_score=4.2)
        score_chain(db, chain_id, session_id)
        score_session(db, session_id)

        resp = client.get("/api/v1/evaluation/sessions/scores",
                          params={"min_score": 4.0})
        assert resp.status_code == 200
        data = resp.json()
        assert any(s["session_id"] == session_id for s in data)
